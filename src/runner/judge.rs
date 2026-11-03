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
        Actor, ActorKind, Definition, Service, VersionedTask, delivery::Stage, execution::Run,
        preferences::PlannerApproval,
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
    let model = role_model(run, "remediation").expect("preflight models");
    let provider = Provider::of(model)?;
    let step = service.start_ai_step(
        run,
        "remediation",
        model,
        &format!("{}#read-only-judge", provider.auth_scope()?),
        false,
    )?;
    service.output(
        run,
        "system",
        &format!(
            "Fix planning: {}/{} · read-only",
            model.provider, model.model
        ),
    )?;
    let (evidence, normal_cycle) = evidence(service, run, ctx, failed)?;
    let (prompt, schema) = request(run, &evidence, normal_cycle);
    let launch = provider.launch(
        model,
        step.provider_session.as_deref(),
        Access::ReadOnly,
        false,
        &schema,
        prompt,
    );
    let invocation = provider_invocation(
        provider,
        launch,
        Access::ReadOnly,
        &ctx.workspace.root,
        ctx.paths,
        ctx.scratch_root(),
        ctx.workspace.worker_lock.clone(),
    )?;
    let mut stream = provider::Stream::new(provider);
    if let Some(outcome) = provider_turn(
        service,
        run,
        &step,
        ctx,
        &invocation,
        &mut stream,
        "remediation",
    )? {
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
    if normal_cycle && matches!(judgment, agent::Judgment::Ask { .. }) {
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

/// What the judge reads about attempt `failed`, and whether this is a normal review/fix
/// round of a live workflow rather than a failed task.
fn evidence(
    service: &Service,
    run: &Run,
    ctx: &Context<'_>,
    failed: i64,
) -> Result<(String, bool)> {
    let mut evidence = format!(
        "{}\nRecorded answers for this attempt:\n{}\n{}",
        service.attempt_evidence(failed, &ctx.paths.state)?,
        service.answers(run)?,
        service.failure_decisions(run.task)?,
    );
    let progress = service.progress(run)?;
    let normal_cycle = progress.as_ref().is_some_and(|p| p.round > 0);
    if let Some(progress) = progress {
        evidence.push_str(&format!(
            "\nCurrent workflow state: {}\n{}\n",
            progress.label(),
            progress.context
        ));
    }
    Ok((evidence, normal_cycle))
}

/// The judge's prompt and response schema. During normal rounds the owner is not asked.
fn request(run: &Run, evidence: &str, normal_cycle: bool) -> (String, serde_json::Value) {
    let mut prompt = agent::judge_prompt(run, evidence);
    let mut schema = agent::judge_schema();
    if normal_cycle {
        prompt.push_str("\nThis is a normal review/fix round within the same active task, not a failed task. Revalidate the current findings; preserve the goal and useful work. Plan fixes only for critical findings and drop minor ones. Cost to result matters most, then time: prefer repair while the worker is converging. You may revise the task or insert prerequisites only if needed, retaining the full goal and acceptance bar. The repair counter follows the original task across automatic revisions and prerequisite handoffs. Do not ask the owner during these rounds.");
        schema["properties"]["judgment"]["anyOf"]
            .as_array_mut()
            .expect("judge alternatives")
            .retain(|choice| choice["properties"]["action"]["enum"][0] != "ask");
    }
    (prompt, schema)
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
