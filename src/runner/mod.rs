mod checks;
pub(crate) mod codex;
mod output;
pub(crate) mod process;
pub mod remediation;
mod workspace;

use crate::{
    application::{
        Service, VersionedTask,
        execution::{Run, Step},
        now_ms, require,
    },
    instance::Paths,
    storage::{Error, Result},
};
use process::{Invocation, Message, Worker};
use std::{
    io::{BufRead, Read, Seek, SeekFrom, Write},
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use workspace::Workspace;

#[derive(Default)]
pub struct Options {
    pub queue: bool,
    pub notify: bool,
    pub recover: bool,
    pub confirm_worker_cleanup: bool,
    pub recover_target: Option<VersionedTask>,
}
enum Outcome {
    Passed(String),
    Failed(&'static str, String),
    Paused,
    Question(String),
    RateLimit,
    /// The failure judge changed the plan (task revised, work inserted, model escalated);
    /// this attempt ends without spending budget, the plan is applied, and the loop goes on.
    Replanned(codex::Judgment),
}
struct Context<'a> {
    paths: &'a Paths,
    workspace: &'a Workspace,
    codex: PathBuf,
    stop: Arc<AtomicBool>,
    cursor: i64,
    started: Instant,
    prior_ms: i64,
}
fn executable(name: &str) -> Result<PathBuf> {
    std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()),
    )
    .map(|p| p.join(name))
    .find(|p| p.is_file())
    .ok_or_else(|| Error::Conflict(format!("{name} executable not found on PATH")))?
    .canonicalize()
    .map_err(Into::into)
}
pub fn run(service: &mut Service, paths: &Paths, options: &Options) -> Result<String> {
    if let Some(expected) = options.recover_target {
        let current = service.current_task()?;
        require(
            current.is_some_and(|t| t.id == expected.id && t.version == expected.version),
            "Current task changed; reopen Recover",
        )?;
    }
    let workspace = Workspace::acquire_with_cleanup_confirmation(
        &service.project_root()?,
        options.recover && options.confirm_worker_cleanup,
    )?;
    if options.recover {
        if options.confirm_worker_cleanup
            && service
                .current_task()?
                .is_none_or(|t| !matches!(t.status.as_str(), "running" | "paused" | "waiting"))
        {
            return Ok("Worker cleanup confirmed; retry current explicitly".into());
        }
        service.recover_run(options.recover_target)?;
        return Ok("Recovered abandoned attempt; retry explicitly".into());
    }
    let codex = executable("codex")?;
    let stop = process::stopped()?;
    let checks = service.checks()?;
    require(
        !checks.is_empty(),
        "Configure at least one real check: check-set build --command 'make check'",
    )?;
    let mut notified = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok("Runner stopped; no next task started".into());
        }
        let Some(task) = service.current_task()? else {
            return Ok("Queue complete".into());
        };
        if notified
            && (task.definition().definition.kind == crate::application::Kind::Human
                || task.definition().lifecycle != "ready")
        {
            return Ok("Queue stopped for human work or approval".into());
        }
        require(
            task.status != "running",
            "An attempt is already running. If its supervisor died, use recover after inspecting the workspace",
        )?;
        require(
            task.definition().definition.kind == crate::application::Kind::Ai,
            "Current task requires human acknowledgment",
        )?;
        for model in &task.definition().definition.models {
            require(
                model.as_ref().is_some_and(|m| m.provider == "codex"),
                "Configure all four roles with provider codex before execution",
            )?;
        }
        let base = workspace.head()?;
        require(
            workspace.clean()? || service.attempt_base(task.id)?.as_deref() == Some(&base),
            "Workspace has uncommitted work from outside this task; commit or preserve it before switching tasks",
        )?;
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
        )?;
        if options.notify && !notified {
            println!("ready");
            std::io::stdout().flush()?;
        }
        notified = true;
        let mut context = Context {
            paths,
            workspace: &workspace,
            codex: codex.clone(),
            stop: Arc::clone(&stop),
            cursor,
            started: Instant::now(),
            prior_ms: run.active_ms,
        };
        // Only an attempt that directly follows a failure is a repair; a judge-replanned
        // attempt ends as interrupted, so the next one starts fresh on the new plan.
        let repair = task.attempts.last().is_some_and(|a| a.state == "failed");
        service.output(
            &run,
            "system",
            if repair {
                "Remediation started with configured remediation model"
            } else {
                "Execution started; results require checks, review and validation"
            },
        )?;
        let result = service.checks().and_then(|checks| {
            require(
                !checks.is_empty(),
                "Configure at least one enabled project check",
            )?;
            pipeline(service, &run, &mut context, &checks, repair)
        });
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => Outcome::Failed("environment", error.to_string()),
        };
        let outcome = if matches!(outcome, Outcome::Passed(_))
            && (stop.load(Ordering::Relaxed) || service.control_cursor(&run)? > context.cursor)
        {
            Outcome::Paused
        } else {
            outcome
        };
        let outcome = match outcome {
            Outcome::Passed(summary) => match workspace.checkpoint(run.task, run.attempt, &base) {
                Ok(head) => {
                    service.finish_run(&run, "succeeded", None, &summary, Some(&head))?;
                    if !options.queue {
                        return Ok(summary);
                    }
                    continue;
                }
                Err(error) => Outcome::Failed("environment", error.to_string()),
            },
            other => other,
        };
        match outcome {
            Outcome::RateLimit => {
                service.rate_wait(&run, "Provider rate limited; session retained")?;
                if options.queue && wait_for_backoff(service, &run, &context)? {
                    continue;
                }
                service.release_supervisor(&run)?;
                return Ok(
                    "Waiting for rate limit backoff; resume the same attempt after 60 seconds"
                        .into(),
                );
            }
            Outcome::Paused => {
                service.finish_run(
                    &run,
                    "paused",
                    None,
                    "Paused; useful work and session retained",
                    None,
                )?;
                service.release_supervisor(&run)?;
                return Ok("Paused".into());
            }
            Outcome::Question(question) => {
                service.finish_run(&run, "waiting", None, &question, None)?;
                service.release_supervisor(&run)?;
                return Ok(format!("Waiting for answer: {question}"));
            }
            Outcome::Replanned(judgment) => {
                let message = describe(&judgment);
                service.finish_run(&run, "interrupted", Some("interrupted"), &message, None)?;
                apply_judgment(service, &run, paths, &workspace, judgment)?;
                if stop.load(Ordering::Relaxed) {
                    return Ok(message);
                }
                continue;
            }
            Outcome::Failed(category, mut message) => {
                let exhausted = coding_failure(category)
                    && run
                        .definition
                        .attempt_limit
                        .is_some_and(|limit| task.attempts_used + 1 >= limit);
                if exhausted
                    && !stop.load(Ordering::Relaxed)
                    && service.control_cursor(&run)? <= context.cursor
                {
                    // Diagnose the final failure while this attempt still owns its worker.
                    // The coding limit stops another repair, not read-only diagnosis.
                    let diagnosis = match judge_attempt(service, &run, &mut context, run.attempt) {
                        Ok(Outcome::Passed(plan)) => format!("Next repair plan: {plan}"),
                        Ok(Outcome::Replanned(proposal)) => format!(
                            "Proposed recovery: {}",
                            serde_json::to_string(&proposal).expect("judgment JSON")
                        ),
                        Ok(Outcome::Question(question)) => {
                            format!("Product decision needed: {question}")
                        }
                        Ok(Outcome::Failed(_, error)) => format!("Diagnosis failed: {error}"),
                        Ok(Outcome::Paused) => "Diagnosis interrupted by pause".into(),
                        Ok(Outcome::RateLimit) => {
                            "Diagnosis deferred by provider rate limit".into()
                        }
                        Err(error) => format!("Diagnosis unavailable: {error}"),
                    };
                    message.push_str(&format!("\n{diagnosis}\nAutomatic repair stopped at the configured attempt limit; no additional worker was started."));
                    service.output(&run, "system", &diagnosis)?;
                }
                service.finish_run(&run, "failed", Some(category), &message, None)?;
                let next = service
                    .current_task()?
                    .ok_or_else(|| Error::Conflict("Current task disappeared".into()))?;
                let budget = next.definition().definition.attempt_limit;
                if coding_failure(category)
                    && budget.is_none_or(|n| next.attempts_used < n)
                    && !stop.load(Ordering::Relaxed)
                {
                    continue;
                }
                return Err(Error::Conflict(format!(
                    "Task #{} stopped ({category}){}: {message}",
                    run.task,
                    if budget.is_some_and(|n| next.attempts_used >= n) {
                        format!(
                            "; automatic repair budget exhausted ({}/{})",
                            next.attempts_used,
                            budget.unwrap()
                        )
                    } else {
                        String::new()
                    },
                )));
            }
            Outcome::Passed(_) => unreachable!(),
        }
    }
}

fn coding_failure(category: &str) -> bool {
    matches!(category, "check" | "review" | "validation" | "agent")
}

pub fn supervisor_alive(service: &Service) -> Result<bool> {
    Workspace::supervisor_alive(&service.project_root()?)
}

fn wait_for_backoff(service: &mut Service, run: &Run, ctx: &Context<'_>) -> Result<bool> {
    service.output(run,"system","Automatic rate-limit backoff; Pause stops the scheduler without discarding the wait or session")?;
    loop {
        if ctx.stop.load(Ordering::Relaxed) || service.control_cursor(run)? > ctx.cursor {
            service.output(
                run,
                "system",
                "Automatic continuation stopped; resume eligibility is retained",
            )?;
            return Ok(false);
        }
        let Some(task) = service.current_task()? else {
            return Ok(false);
        };
        if task.id != run.task {
            return Ok(false);
        }
        if task
            .attempts
            .last()
            .and_then(|a| a.resume_at_ms)
            .is_some_and(|at| now_ms() >= at)
        {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn pipeline(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    checks: &[crate::application::execution::Check],
    repair: bool,
) -> Result<Outcome> {
    let answers = service.answers(run)?;
    let prior = service
        .task(run.task)?
        .map(|t| {
            t.attempts
                .iter()
                .rev()
                .filter_map(|a| a.outcome.as_deref())
                .take(1)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let mut context = format!(
        "{answers}\nPrior failure context (scope only; not proof of current success): {prior}"
    );
    let role = if repair { "remediation" } else { "execution" };
    if repair {
        match judge(service, run, ctx)? {
            Outcome::Passed(plan) => {
                context.push_str(&format!("\nRepair plan from the failure judge:\n{plan}"));
            }
            other => return Ok(other),
        }
    }
    let mut evidence = match ai(service, run, ctx, role, &context)? {
        Outcome::Passed(s) => {
            format!(
                "{context}\nCurrent attempt executor assessment (not independent evidence):\n{s}"
            )
        }
        other => return Ok(other),
    };
    match checks::run(service, run, ctx, checks)? {
        Outcome::Passed(checks) => evidence.push_str(&checks),
        other => return Ok(other),
    }
    for role in ["review", "validation"] {
        match ai(service, run, ctx, role, &evidence)? {
            Outcome::Passed(s) => evidence.push_str(&format!("\n{role}: {s}")),
            other => return Ok(other),
        };
    }
    Ok(Outcome::Passed(
        "Checks, independent review and goal validation passed; checkpoint saved".into(),
    ))
}
fn ai(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    role: &'static str,
    evidence: &str,
) -> Result<Outcome> {
    for retry in 0..=2 {
        let outcome = ai_turn(service, run, ctx, role, evidence, None)?;
        if !matches!(outcome, Outcome::Failed("transport", _)) || retry == 2 {
            return Ok(outcome);
        }
        service.output(
            run,
            "system",
            &format!(
                "Provider connection failed · automatic retry {}/2 · same attempt, work retained",
                retry + 1
            ),
        )?;
        let until = Instant::now() + Duration::from_secs(1 << retry);
        while Instant::now() < until {
            if ctx.stop.load(Ordering::Relaxed) || service.control_cursor(run)? > ctx.cursor {
                return Ok(Outcome::Paused);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    unreachable!("bounded provider retries")
}
/// One provider turn. A first `blocked` answer is nudged once in the same session, because
/// permission and sandbox limits are not questions for the owner; only a repeated, genuine
/// product question reaches the owner, and reviewers that still refuse to decide stop the task.
fn ai_turn(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    role: &'static str,
    evidence: &str,
    nudge: Option<&str>,
) -> Result<Outcome> {
    let index = crate::application::ROLES
        .iter()
        .position(|r| *r == role)
        .expect("role");
    let model = run.definition.models[index]
        .as_ref()
        .expect("preflight models");
    let scope = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
        .ok_or_else(|| Error::Conflict("Codex auth home is not configured".into()))?;
    let step =
        service.start_ai_step(run, role, model, &scope.to_string_lossy(), nudge.is_some())?;
    service.output(
        run,
        "system",
        &format!(
            "{role}: {}/{} · {}",
            model.provider,
            model.model,
            if nudge.is_some() {
                "nudge: decide without asking"
            } else if step.provider_session.is_some() {
                "resume session"
            } else {
                "new session"
            }
        ),
    )?;
    let invocation = Invocation {
        executable: ctx.codex.clone(),
        args: codex::args(model, &step, role)?,
        cwd: ctx.workspace.root.clone(),
        prompt: nudge.map_or_else(
            || codex::prompt(run, role, evidence),
            |question| codex::nudge(role, question),
        ),
        schema: Some(codex::schema()),
        scratch_root: ctx.paths.cache.join("workers"),
        worker_lock: ctx.workspace.worker_lock.clone(),
    };
    let mut stream = codex::Stream::default();
    match execute(
        service,
        run,
        &step,
        ctx,
        &invocation,
        None,
        true,
        &mut stream,
    )? {
        Outcome::Passed(_) => {}
        Outcome::Failed(category, message) => {
            service.finish_step(&step, false, &message, None)?;
            return Ok(Outcome::Failed(category, message));
        }
        other => return Ok(other),
    }
    let assessment = match stream.assessment(run.definition.criteria.len()) {
        Ok(a) => a,
        Err(e) => {
            service.finish_step(&step, false, &e.to_string(), None)?;
            return Ok(Outcome::Failed("protocol", e.to_string()));
        }
    };
    if assessment.outcome == "blocked" {
        service.finish_step(&step, false, &assessment.summary, None)?;
        let question = assessment.question.expect("validated");
        if nudge.is_none() {
            return ai_turn(service, run, ctx, role, evidence, Some(&question));
        }
        if matches!(role, "review" | "validation") {
            return Ok(Outcome::Failed(
                "protocol",
                format!("{role} asked instead of deciding from the evidence: {question}"),
            ));
        }
        return Ok(Outcome::Question(question));
    }
    let passed = assessment.outcome == "complete" && assessment.criteria.iter().all(|c| c.passed);
    for criterion in &assessment.criteria {
        service.criterion(
            run,
            &step,
            criterion.ordinal,
            criterion.passed,
            &criterion.evidence,
        )?;
    }
    service.finish_step(&step, passed, &assessment.summary, None)?;
    Ok(if passed {
        Outcome::Passed(assessment.summary)
    } else {
        Outcome::Failed(
            if matches!(role, "review" | "validation") {
                role
            } else {
                "agent"
            },
            assessment.summary,
        )
    })
}
/// The failure judge: a read-only turn of the remediation model that decides whether the
/// next worker repairs, the task is rewritten, prerequisite work is inserted, the model is
/// escalated, or the owner must decide. Applied immediately; nothing runs twice.
fn judge(service: &mut Service, run: &Run, ctx: &mut Context<'_>) -> Result<Outcome> {
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
fn judge_attempt(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    failed: i64,
) -> Result<Outcome> {
    let model = run.definition.models[3].as_ref().expect("preflight models");
    let scope = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
        .ok_or_else(|| Error::Conflict("Codex auth home is not configured".into()))?;
    let step = service.start_ai_step(run, "remediation", model, &scope.to_string_lossy(), false)?;
    service.output(
        run,
        "system",
        &format!(
            "failure judge: {}/{} · read-only",
            model.provider, model.model
        ),
    )?;
    let evidence = service.attempt_evidence(failed, &ctx.paths.state)?;
    let invocation = Invocation {
        executable: ctx.codex.clone(),
        // Sandbox follows the validation role: read-only by design.
        args: codex::args(model, &step, "validation")?,
        cwd: ctx.workspace.root.clone(),
        prompt: codex::judge_prompt(run, &evidence),
        schema: Some(codex::judge_schema()),
        scratch_root: ctx.paths.cache.join("workers"),
        worker_lock: ctx.workspace.worker_lock.clone(),
    };
    let mut stream = codex::Stream::default();
    match execute(
        service,
        run,
        &step,
        ctx,
        &invocation,
        None,
        true,
        &mut stream,
    )? {
        Outcome::Passed(_) => {}
        Outcome::Failed(category, message) => {
            service.finish_step(&step, false, &message, None)?;
            return Ok(Outcome::Failed(category, message));
        }
        other => return Ok(other),
    }
    if !stream.completed || stream.failed {
        let message = "Failure judge ended without a complete turn".to_owned();
        service.finish_step(&step, false, &message, None)?;
        return Ok(Outcome::Failed("protocol", message));
    }
    let judgment = match codex::parse_judgment(stream.last_message.as_deref().unwrap_or("")) {
        Ok(j) => j,
        Err(e) => {
            service.finish_step(&step, false, &e.to_string(), None)?;
            return Ok(Outcome::Failed("protocol", e.to_string()));
        }
    };
    let (summary, outcome) = match judgment {
        codex::Judgment::Repair { plan, reason } => {
            (format!("Repair: {reason}"), Outcome::Passed(plan))
        }
        codex::Judgment::Ask { question, reason } => {
            service.finish_step(
                &step,
                false,
                &format!("Owner decision needed: {reason}"),
                None,
            )?;
            return Ok(Outcome::Question(question));
        }
        other => (describe(&other), Outcome::Replanned(other)),
    };
    service.finish_step(&step, true, &summary, None)?;
    Ok(outcome)
}

fn describe(judgment: &codex::Judgment) -> String {
    match judgment {
        codex::Judgment::Repair { reason, .. } => format!("Repair: {reason}"),
        codex::Judgment::Revise { reason, .. } => {
            format!("Task revised by the failure judge: {reason}")
        }
        codex::Judgment::Insert { tasks, reason } => format!(
            "{} prerequisite task(s) inserted by the failure judge: {reason}",
            tasks.len()
        ),
        codex::Judgment::Escalate { model, reason } => format!(
            "Execution escalated to {}/{} by the failure judge: {reason}",
            model.provider, model.model
        ),
        codex::Judgment::Ask { reason, .. } => format!("Owner decision needed: {reason}"),
    }
}

/// Applied once the judged attempt has ended, so ordinary revision and insertion rules hold.
fn apply_judgment(
    service: &mut Service,
    run: &Run,
    paths: &Paths,
    workspace: &Workspace,
    judgment: codex::Judgment,
) -> Result<()> {
    let task = service
        .task(run.task)?
        .ok_or_else(|| Error::Conflict("Current task disappeared".into()))?;
    let target = VersionedTask {
        id: task.id,
        version: task.version,
    };
    let ready = service.preferences(&paths.config).is_ok_and(|p| {
        p.planner_approval != crate::application::preferences::PlannerApproval::ReviewRequired
    });
    let judge = crate::application::Actor {
        kind: crate::application::ActorKind::Ai,
        name: "judge".into(),
    };
    match judgment {
        codex::Judgment::Revise {
            title,
            goal,
            criteria,
            reason,
        } => {
            let mut definition = run.definition.clone();
            definition.title = title;
            definition.goal = goal;
            definition.criteria = criteria;
            service.revise_as(target, &definition, ready, &judge, Some(&reason))?;
        }
        codex::Judgment::Insert { tasks, reason } => {
            let defaults = service.defaults()?;
            let repairs: Vec<_> = tasks
                .iter()
                .map(|t| crate::application::Definition {
                    title: t.title.clone(),
                    goal: t.goal.clone(),
                    kind: crate::application::Kind::Ai,
                    criteria: t.criteria.clone(),
                    models: defaults.clone(),
                    attempt_limit: Some(3),
                    timeout_ms: run.definition.timeout_ms,
                })
                .collect();
            remediation::preserve(service, workspace, target)?;
            service.apply_remediation(target, None, &repairs, &reason, ready)?;
        }
        codex::Judgment::Escalate { model, reason } => {
            service.apply_remediation(target, Some(model), &[], &reason, true)?;
        }
        codex::Judgment::Repair { .. } | codex::Judgment::Ask { .. } => {}
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn execute(
    service: &mut Service,
    run: &Run,
    step: &Step,
    ctx: &mut Context<'_>,
    spec: &Invocation,
    timeout: Option<i64>,
    ai: bool,
    stream: &mut codex::Stream,
) -> Result<Outcome> {
    let mut worker = Worker::spawn(&ctx.paths.executable, spec)?;
    let started = Instant::now();
    let mut interrupted = None;
    let mut last_tick = Instant::now();
    let mut stdout = output::Utf8Stream::default();
    let mut stderr = output::Utf8Stream::default();
    loop {
        let active = ctx.prior_ms + ctx.started.elapsed().as_millis() as i64;
        if interrupted.is_none() {
            if ctx.stop.load(Ordering::Relaxed) || service.control_cursor(run)? > ctx.cursor {
                interrupted = Some(Outcome::Paused);
                worker.stop();
            } else if timeout.is_some_and(|n| started.elapsed().as_millis() >= n as u128)
                || run.definition.timeout_ms.is_some_and(|n| active >= n)
            {
                interrupted = Some(Outcome::Failed(
                    "timeout",
                    "Active time limit reached; work retained".into(),
                ));
                worker.stop();
            }
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            service.active_time(run, active)?;
            last_tick = Instant::now();
        }
        match worker.messages.recv_timeout(Duration::from_millis(40)) {
            Ok(Message::Output {
                stream: source,
                bytes,
            }) => {
                if ai && source == "stdout" {
                    for event in stream.feed(&bytes)? {
                        match event {
                            codex::Event::Session(id) => service.record_session(step, &id)?,
                            codex::Event::Usage(usage) => service.record_usage(step, &usage)?,
                            codex::Event::Text(text) => service.output(run, "stdout", &text)?,
                        }
                    }
                } else {
                    let decoder = if source == "stdout" {
                        &mut stdout
                    } else {
                        &mut stderr
                    };
                    let text = decoder.feed(&bytes);
                    if !text.is_empty() {
                        service.output(run, &source, &text)?;
                    }
                }
            }
            Ok(Message::Exit { code }) => {
                worker.join()?;
                for (source, decoder) in [("stdout", &mut stdout), ("stderr", &mut stderr)] {
                    let text = decoder.finish();
                    if !text.is_empty() {
                        service.output(run, source, &text)?;
                    }
                }
                service.active_time(run, active)?;
                if interrupted.is_none() && ai && stream.rate_limited {
                    return Ok(Outcome::RateLimit);
                }
                if interrupted.is_none() && !ai {
                    service.finish_step(
                        step,
                        code == 0,
                        &format!("Check exited {code}"),
                        Some(code),
                    )?;
                }
                return Ok(interrupted.unwrap_or_else(|| {
                    if code == 0 {
                        Outcome::Passed("Process completed and scratch cleaned".into())
                    } else {
                        Outcome::Failed(
                            if ai { "transport" } else { "check" },
                            format!(
                                "{} exited {code}; see stored output",
                                if ai { "Codex" } else { "Check" }
                            ),
                        )
                    }
                }));
            }
            Ok(Message::Error(message)) => return Err(Error::Conflict(message)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                worker.join()?;
                return Err(Error::Conflict(
                    "Worker exited without cleanup confirmation".into(),
                ));
            }
        }
    }
}

/// Abandon a paused/waiting attempt while holding the runner lease, so a live supervisor
/// (for example one sleeping through a rate-limit wait) can never lose its attempt underneath it.
pub fn abandon(
    service: &mut Service,
    target: crate::application::VersionedTask,
    reason: &str,
) -> Result<()> {
    let _workspace = Workspace::acquire(&service.project_root()?)?;
    service.abandon_run(target, reason)
}

/// Give up on the whole task; it moves to History as cancelled with its attempts intact.
pub fn abandon_task(
    service: &mut Service,
    target: crate::application::VersionedTask,
    reason: &str,
) -> Result<()> {
    let _workspace = Workspace::acquire(&service.project_root()?)?;
    service.abandon_task(target, reason)
}

/// Launch the exact supervisor executable, never a PATH-selected ktask or a rebuild target.
pub fn background(service: &mut Service, paths: &Paths, queue: bool) -> Result<()> {
    drop(Workspace::acquire(&service.project_root()?)?);
    require(
        !service.checks()?.is_empty(),
        "Configure an enabled check in Configuration > Checks (or CLI check-set)",
    )?;
    if let Some(task) = service.current_task()? {
        for model in &task.definition().definition.models {
            require(
                model.as_ref().is_some_and(|m| m.provider == "codex"),
                "Configure all four task roles with provider codex in Models; defaults only apply to new tasks",
            )?;
        }
    }
    std::fs::create_dir_all(&paths.state)?;
    let log_path = paths
        .state
        .join(format!("runner-{}.log", service.project_id));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_start = log.metadata()?.len();
    let mut command = Command::new(&paths.executable);
    command
        .arg("--project")
        .arg(service.project_root()?)
        .args(["run", "--notify"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log)
        .process_group(0);
    command.arg(if queue { "--queue" } else { "--one" });
    let mut child = command.spawn()?;
    let output = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(output);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let ready = line.trim() == "ready";
        if ready {
            let _ = tx.send(Ok(()));
        }
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
        let _ = child.wait();
        if !ready {
            // Stderr stays attached to a file so a detached runner survives its
            // launching CLI. Read bytes appended since startup, never old errors.
            let error = (|| -> std::io::Result<String> {
                let mut log = std::fs::File::open(log_path)?;
                let end = log.metadata()?.len();
                log.seek(SeekFrom::Start(log_start.max(end.saturating_sub(8192))))?;
                let mut bytes = Vec::new();
                log.take(8192).read_to_end(&mut bytes)?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            })()
            .unwrap_or_default();
            let _ = tx.send(Err(if error.trim().is_empty() {
                format!("Runner exited before startup: {}", line.trim())
            } else {
                error
                    .trim()
                    .strip_prefix("conflict: command conflict: ")
                    .unwrap_or(error.trim())
                    .to_owned()
            }));
        }
    });
    rx.recv_timeout(Duration::from_secs(10))
        .map_err(|_| {
            Error::Conflict(
                "Runner startup still pending; inspect runner log before starting again".into(),
            )
        })?
        .map_err(Error::Conflict)
}
