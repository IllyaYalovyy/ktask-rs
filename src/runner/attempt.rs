//! One attempt of the current task: begun on a verified workspace, driven through its
//! workflow and Git delivery, and settled into what the queue does next.
use super::{
    Context, Outcome, Workspace, backoff, cycle, delivery, holds_own_work, judge, resource_limit,
    workspace,
};
use crate::{
    application::{
        Service, Task, VersionedTask, delivery::Stage, execution::Run, now_ms, require,
        stop::unresolvable,
    },
    instance::Paths,
    storage::{Error, Result},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

/// What every attempt of one supervisor run shares.
pub(super) struct Launch<'a> {
    pub(super) paths: &'a Paths,
    pub(super) workspace: &'a Workspace,
    pub(super) stop: &'a Arc<AtomicBool>,
    /// Run the queue: a passed attempt goes on to the next task.
    pub(super) queue: bool,
}

/// What the queue does once an attempt has settled.
pub(super) enum Then {
    /// Go on with the current task, whichever it now is.
    Continue,
    /// Go on, opening the next attempt with the supervisor's own repair.
    Repair,
    /// End the run with this message.
    Return(String),
}

/// A begun attempt, owned by this supervisor.
pub(super) struct Begun {
    run: Run,
    opening: cycle::Opening,
    /// The control cursor the attempt resumes from.
    cursor: i64,
    /// Whether the attempt's delivery settings could be pinned.
    delivery: Result<()>,
}

/// Starts the task's next attempt, or resumes its live one, from `opening`.
pub(super) fn begin(
    service: &mut Service,
    workspace: &Workspace,
    task: &Task,
    opening: cycle::Opening,
) -> Result<Begun> {
    let base = workspace.head()?;
    require_own_work(service, workspace, task, opening, &base)?;
    let token = format!("{}:{}:{}", service.project_id, std::process::id(), now_ms());
    let cursor = if matches!(task.status.as_str(), "paused" | "waiting") {
        service.pause_cursor(task.attempts.last().expect("live attempt").id)?
    } else {
        0
    };
    let run = service.begin_run(
        VersionedTask {
            id: task.id,
            version: task.version,
        },
        &token,
        &base,
        opening != cycle::Opening::Repair,
    )?;
    // A live attempt blocks configuration changes before settings are read or Git mutates.
    let delivery = pin_delivery(service, workspace, &run);
    Ok(Begun {
        run,
        opening,
        cursor,
        delivery,
    })
}

/// A fresh Run of a Pending task owns no uncommitted work: Stop stashed it. Only a Retry,
/// Resume or repair may carry the task's own edits into the attempt.
fn require_own_work(
    service: &Service,
    workspace: &Workspace,
    task: &Task,
    opening: cycle::Opening,
    base: &str,
) -> Result<()> {
    let own_work = !matches!(opening, cycle::Opening::Fresh { .. })
        || holds_own_work(task)
        || service.inherits_delivery_handoff(task.id)?;
    require(
        workspace.clean()?
            || own_work
                && (service.pending_delivery_handoff(task.id)?
                    || workspace::retains_work_from(
                        &workspace.root,
                        service.attempt_base(task.id)?.as_deref(),
                        base,
                    )?
                    || service
                        .current_delivery_stage(task.id)?
                        .is_some_and(|s| s != Stage::Work)),
        "Workspace has uncommitted work from outside this task; commit or preserve it before switching tasks",
    )
}

/// Pins where the attempt delivers: a handed-over delivery, else the current settings.
fn pin_delivery(service: &mut Service, workspace: &Workspace, run: &Run) -> Result<()> {
    if service.attempt_delivery(run).is_ok() {
        return Ok(());
    }
    let config = service.delivery_config()?;
    let destination = if config.mode == "local" {
        String::new()
    } else {
        workspace::git(
            &workspace.root,
            &["remote", "get-url", "--push", &config.remote],
        )?
    };
    let branch = if config.mode == "pr" {
        Some(service.delivery_branch(&config)?)
    } else {
        None
    };
    if service.claim_delivery_handoff(run, &config, branch.as_deref(), &destination)? {
        return Ok(());
    }
    service.ensure_attempt_delivery(run, &config, branch.as_deref(), &destination)?;
    Ok(())
}

/// Drives the begun attempt to its outcome and settles it.
pub(super) fn run(service: &mut Service, launch: &Launch<'_>, begun: Begun) -> Result<Then> {
    let Begun {
        run,
        opening,
        cursor,
        delivery,
    } = begun;
    let mut context = Context {
        paths: launch.paths,
        workspace: launch.workspace,
        stop: Arc::clone(launch.stop),
        cursor,
        started: Instant::now(),
        prior_ms: run.active_ms,
        earlier_task_ms: service.earlier_task_active_ms(&run)?,
    };
    let result = drive(service, &run, &mut context, opening, delivery);
    // A transport error during a provider wait belongs to the same outage.
    let had_rate_wait =
        matches!(&result, Ok(Outcome::Failed("transport", _))) && service.rate_waits(&run)? > 0;
    let outcome = settle_result(result, had_rate_wait);
    if !launch.queue
        && let Outcome::Passed(report) = &outcome
    {
        return Ok(Then::Return(report.clone()));
    }
    let outcome = if matches!(outcome, Outcome::Passed(_)) && context.interrupted(service, &run)? {
        Outcome::Paused
    } else {
        outcome
    };
    if matches!(outcome, Outcome::Passed(_)) {
        return Ok(Then::Continue);
    }
    // Include orchestration and operational backoff, even when no worker launched.
    // Freeze the clock before handing ownership to a pause, wait, or later attempt.
    context.persist_active_time(service, &run)?;
    settle(service, launch, &run, &context, outcome)
}

fn settle_result(result: Result<Outcome>, had_rate_wait: bool) -> Outcome {
    match result {
        Ok(Outcome::Failed("transport", _)) if had_rate_wait => Outcome::RateLimit(None),
        Ok(outcome) => outcome,
        Err(error) => match resource_limit(&error) {
            Some(reason) => Outcome::DeliveryBlocked(reason.into()),
            None => Outcome::Failed("environment", error.to_string()),
        },
    }
}

/// The attempt's work: its workflow until verified, then checkpoint and delivery, repeated
/// while delivery sends the work back for new proof.
fn drive(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    opening: cycle::Opening,
    delivery: Result<()>,
) -> Result<Outcome> {
    service.output(run, "system", opening_notice(opening, run))?;
    if let Err(error) = delivery {
        return Ok(Outcome::DeliveryBlocked(error.to_string()));
    }
    let mut delivery_retries = 0_u32;
    loop {
        match delivery::prepare(service, run, ctx) {
            Ok(false) => return Ok(Outcome::Paused),
            Ok(true) => {}
            Err(error) => {
                if let Some(outcome) =
                    delivery::retry(service, run, ctx, &error, &mut delivery_retries)?
                {
                    return Ok(outcome);
                }
                continue;
            }
        }
        if let Err(error) = delivery::reconcile_resume(service, run, ctx) {
            return Ok(Outcome::DeliveryBlocked(error.to_string()));
        }
        let summary = match verify(service, run, ctx, opening)? {
            Outcome::Passed(summary) => summary,
            other => return Ok(other),
        };
        if ctx.interrupted(service, run)? {
            return Ok(Outcome::Paused);
        }
        let head = match checkpoint(service, run, ctx)? {
            Ok(head) => head,
            Err(blocked) => return Ok(blocked),
        };
        if let Some(outcome) = deliver(service, run, ctx, &summary, &head, &mut delivery_retries)? {
            return Ok(outcome);
        }
    }
}

fn opening_notice(opening: cycle::Opening, run: &Run) -> &'static str {
    match opening {
        cycle::Opening::Repair => "Repair started with the configured remediation model",
        cycle::Opening::Retry if !run.definition.kind.runs_checks() => {
            "Retry started from the kept work; review and validation follow"
        }
        cycle::Opening::Retry => "Retry started from the kept work; checks run first",
        cycle::Opening::Fresh { .. } if !run.definition.kind.runs_checks() => {
            "Docs execution started; no project checks, results require review and validation"
        }
        cycle::Opening::Fresh { .. } => {
            "Execution started; results require checks, review and validation"
        }
    }
}

/// The workflow until checks, review and validation pass. Once delivery has begun, an
/// operational failure blocks delivery instead of failing the attempt.
fn verify(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    opening: cycle::Opening,
) -> Result<Outcome> {
    let result = service.checks().and_then(|checks| {
        require(
            !checks.is_empty() || !run.definition.kind.runs_checks(),
            "Configure at least one enabled project check",
        )?;
        cycle::run(service, run, ctx, &checks, opening)
    });
    match result {
        Ok(Outcome::Failed("transport" | "environment" | "protocol", message))
            if service.attempt_delivery(run)?.stage != Stage::Work =>
        {
            Ok(Outcome::DeliveryBlocked(message))
        }
        Err(error) if service.attempt_delivery(run)?.stage != Stage::Work => {
            Ok(Outcome::DeliveryBlocked(error.to_string()))
        }
        other => other,
    }
}

/// Commits the verified tree. The new head, or the outcome when Git refuses.
fn checkpoint(
    service: &mut Service,
    run: &Run,
    ctx: &Context<'_>,
) -> Result<std::result::Result<String, Outcome>> {
    let verified = service
        .progress(run)?
        .ok_or_else(|| Error::Conflict("Missing verified workflow state".into()))?
        .verified_tree;
    let delivery = service.attempt_delivery(run)?;
    let expected_head = delivery.checkpoint_head.clone().unwrap_or(
        service
            .attempt_base(run.task)?
            .ok_or_else(|| Error::Conflict("Attempt baseline missing".into()))?,
    );
    service.delivery_stage(run, Stage::Checkpointing, Some(&expected_head), None, None)?;
    let head = match ctx.workspace.checkpoint(
        run.task,
        run.attempt,
        &expected_head,
        &verified,
        matches!(
            delivery.stage,
            Stage::Checkpointed
                | Stage::Integrating
                | Stage::Verified
                | Stage::Pushed
                | Stage::PrReady
        ),
    ) {
        Ok(head) => head,
        Err(error) => return Ok(Err(Outcome::DeliveryBlocked(error.to_string()))),
    };
    service.delivery_stage(run, Stage::Checkpointed, Some(&head), None, None)?;
    Ok(Ok(head))
}

/// Publishes the checkpoint. Some outcome ends the attempt; None sends the work back
/// through the workflow, or retries delivery after its backoff.
fn deliver(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    summary: &str,
    head: &str,
    retries: &mut u32,
) -> Result<Option<Outcome>> {
    match delivery::publish(service, run, ctx, summary, head) {
        Ok(delivery::DeliveryOutcome::Done(report)) => {
            ctx.persist_active_time(service, run)?;
            service.finish_run(
                run,
                "succeeded",
                None,
                &report,
                Some(&ctx.workspace.head()?),
            )?;
            Ok(Some(Outcome::Passed(report)))
        }
        Ok(delivery::DeliveryOutcome::Retry) => {
            *retries = 0;
            Ok(None)
        }
        Ok(delivery::DeliveryOutcome::Paused) => Ok(Some(Outcome::Paused)),
        Ok(delivery::DeliveryOutcome::Exhausted(reason)) => Ok(Some(cycle::unresolved(&reason))),
        Err(error) => delivery::retry(service, run, ctx, &error, retries),
    }
}

/// Hands an attempt that did not pass to what follows it: a limit wait, the owner, a
/// replan or the supervisor's own repair.
fn settle(
    service: &mut Service,
    launch: &Launch<'_>,
    run: &Run,
    ctx: &Context<'_>,
    outcome: Outcome,
) -> Result<Then> {
    match outcome {
        // Never fail a task for provider limits: wait for the provider's own reset
        // time when it names one, else back off, then resume the same attempt.
        Outcome::RateLimit(reset_at) => wait_out_limit(service, run, ctx, reset_at),
        Outcome::Paused => {
            release(
                service,
                run,
                "paused",
                "Paused; useful work and session retained",
            )?;
            Ok(Then::Return("Paused".into()))
        }
        Outcome::DeliveryBlocked(reason) => {
            release(service, run, "paused", &reason)?;
            Ok(Then::Return(format!("Paused: {reason}")))
        }
        Outcome::Question(question) => {
            release(service, run, "waiting", &question)?;
            Ok(Then::Return(format!("Waiting for answer: {question}")))
        }
        Outcome::Replanned(judgment) => replan(service, launch, run, judgment),
        Outcome::Failed(category, message) => fail(service, launch, run, category, &message),
        Outcome::Passed(_) | Outcome::Changes { .. } => unreachable!(),
    }
}

/// Ends the attempt in `state` and hands it back: no supervisor owns it now.
fn release(service: &mut Service, run: &Run, state: &str, report: &str) -> Result<()> {
    service.finish_run(run, state, None, report, None)?;
    service.release_supervisor(run)
}

fn wait_out_limit(
    service: &mut Service,
    run: &Run,
    ctx: &Context<'_>,
    reset_at: Option<i64>,
) -> Result<Then> {
    service.rate_wait(run, reset_at)?;
    match backoff::wait_for_limit_reset(service, run, ctx.cursor, &ctx.stop) {
        Ok(true) => return Ok(Then::Continue),
        Err(error) => {
            let Some(reason) = resource_limit(&error) else {
                return Err(error);
            };
            service.pause_wait(run, reason)?;
            service.release_supervisor(run)?;
            return Ok(Then::Return(format!("Paused: {reason}")));
        }
        Ok(false) => {}
    }
    // Paused during the wait: Resume tries again at once.
    service.pause_wait(
        run,
        "Paused during a provider limit wait; Resume tries again now",
    )?;
    service.release_supervisor(run)?;
    Ok(Then::Return("Paused during a provider limit wait".into()))
}

/// The failure judge changed the plan: the attempt ends and the plan is applied, at most
/// ten times per task.
fn replan(
    service: &mut Service,
    launch: &Launch<'_>,
    run: &Run,
    judgment: super::agent::Judgment,
) -> Result<Then> {
    let message = judge::describe(&judgment);
    if service.automatic_replans(run.task)? >= 10 {
        let reason = format!(
            "Automatic recovery exhausted after 10 judge replans; latest judgment: {message}. Work and evidence retained. Edit current or apply manual remediation to continue."
        );
        service.finish_run(run, "failed", Some("protocol"), &reason, None)?;
        return Err(Error::Conflict(reason));
    }
    service.finish_run(run, "interrupted", Some("interrupted"), &message, None)?;
    service.record_automatic_replan(run.task)?;
    judge::apply_judgment(service, run, launch.paths, launch.workspace, judgment)?;
    service.replan_applied(run)?;
    if launch.stop.load(Ordering::Relaxed) {
        return Ok(Then::Return(message));
    }
    Ok(Then::Continue)
}

/// A failed attempt is repaired automatically: the next attempt opens with the read-only
/// failure judge. Only a failure no repair can change stops execution.
fn fail(
    service: &mut Service,
    launch: &Launch<'_>,
    run: &Run,
    category: &'static str,
    message: &str,
) -> Result<Then> {
    if let Err(error) = service.output(
        run,
        "system",
        &format!("Attempt failed ({category}); starting a repair attempt automatically"),
    ) {
        let Some(reason) = resource_limit(&error) else {
            return Err(error);
        };
        release(service, run, "paused", reason)?;
        return Ok(Then::Return(format!("Paused: {reason}")));
    }
    service.finish_run(run, "failed", Some(category), message, None)?;
    let task = service
        .task(run.task)?
        .ok_or_else(|| Error::Conflict("Current task disappeared".into()))?;
    if let Some(stop) = unresolvable(&task) {
        return Err(Error::Conflict(format!(
            "Task #{} stopped ({category}): {} {}",
            run.task, stop.reason, stop.unresolved
        )));
    }
    if launch.stop.load(Ordering::Relaxed) {
        return Ok(Then::Return(
            "Runner stopped; repair attempt not started".into(),
        ));
    }
    Ok(Then::Repair)
}
