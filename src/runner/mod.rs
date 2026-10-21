pub(crate) mod agent;
pub(crate) mod callback;
mod checks;
mod cycle;
mod output;
pub(crate) mod process;
pub(crate) mod provider;
pub mod remediation;
mod reset;
pub(crate) mod workspace;

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
use provider::{Access, Provider};
use std::{
    io::{BufRead, Read, Seek, SeekFrom, Write},
    os::unix::{fs::PermissionsExt, process::CommandExt},
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
    pub continue_session: bool,
    pub recover: bool,
    pub confirm_worker_cleanup: bool,
    pub recover_target: Option<VersionedTask>,
}
enum Outcome {
    Passed(String),
    Failed(&'static str, String),
    Changes {
        report: String,
        critical: bool,
    },
    Paused,
    Question(String),
    /// Provider rate or usage limit, with its own reset time when it names one.
    RateLimit(Option<i64>),
    /// The failure judge changed the plan (task revised, work inserted, model escalated);
    /// this attempt ends without spending budget, the plan is applied, and the loop goes on.
    Replanned(agent::Judgment),
}
struct Context<'a> {
    paths: &'a Paths,
    workspace: &'a Workspace,
    stop: Arc<AtomicBool>,
    cursor: i64,
    started: Instant,
    prior_ms: i64,
    earlier_task_ms: i64,
}
impl Context<'_> {
    fn active_ms(&self) -> i64 {
        self.prior_ms
            .saturating_add(self.started.elapsed().as_millis().min(i64::MAX as u128) as i64)
    }

    fn persist_active_time(&self, service: &Service, run: &Run) -> Result<()> {
        service.active_time(run, self.active_ms())
    }

    fn deadline_outcome(&self, run: &Run) -> Option<Outcome> {
        run.definition
            .timeout_ms
            .filter(|&limit| self.earlier_task_ms.saturating_add(self.active_ms()) >= limit)
            .map(|_| Outcome::Failed(
                "agent",
                "Active time limit reached (configured task-wide limit); work and evidence retained. Increase the task limit explicitly before retrying".into(),
            ))
    }
}
/// Every role names a supported provider whose CLI is installed.
fn connected(definition: &crate::application::Definition) -> Result<()> {
    for model in &definition.models {
        let model = model.as_ref().ok_or_else(|| {
            Error::Conflict(format!(
                "Configure all four task roles in Models ({}); defaults only apply to new tasks",
                provider::SUPPORTED
            ))
        })?;
        Provider::of(model)?.executable()?;
    }
    Ok(())
}
pub(crate) fn executable(name: &str) -> Result<PathBuf> {
    let path = std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()),
    )
    .map(|p| p.join(name))
    .find(|p| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
    .ok_or_else(|| Error::Conflict(format!("{name} executable not found on PATH")))?;
    path.canonicalize().map_err(Into::into)
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
    let mut owns_session = false;
    let result = run_inner(service, paths, options, &workspace, &mut owns_session);
    if !owns_session {
        return result;
    }
    let mut requested = false;
    if let Some(session) = service.execution_session()?
        && session.state != "paused"
    {
        let (state, reason) = match &result {
            Err(error) => {
                let current = service.current_task()?;
                if session.mode == "queue" && current.is_some_and(|t| t.status == "failed") {
                    ("waiting", error.to_string())
                } else {
                    ("stopped", error.to_string())
                }
            }
            Ok(message) => {
                let current = service.current_task()?;
                match current {
                    None => ("completed", message.clone()),
                    Some(task) if task.status == "paused" => ("paused", message.clone()),
                    Some(task) if session.mode == "one" && task.status != "waiting" => {
                        ("stopped", "Run one completed the current task".into())
                    }
                    Some(task)
                        if task.status == "waiting"
                            || task.status == "failed"
                            || task.definition().lifecycle != "ready"
                            || task.definition().definition.kind
                                == crate::application::Kind::Human =>
                    {
                        ("waiting", message.clone())
                    }
                    _ => ("stopped", message.clone()),
                }
            }
        };
        requested = service.settle_execution_session(state, &reason)?;
    }
    // Keep the lease until the final state is committed. An intervention that
    // arrived during this handoff is retained and claims the next supervisor.
    drop(workspace);
    if requested
        || service
            .execution_session()?
            .is_some_and(|s| s.state == "resume_requested")
    {
        let _ = continue_pending_session(service, paths);
    }
    result
}

fn run_inner(
    service: &mut Service,
    paths: &Paths,
    options: &Options,
    workspace: &Workspace,
    owns_session: &mut bool,
) -> Result<String> {
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
    if options.continue_session {
        require(
            service
                .execution_session()?
                .is_some_and(|s| s.mode == "queue" && s.state == "running"),
            "Queue session is no longer active; use Run to start again",
        )?;
    } else {
        service.start_execution_session(options.queue)?;
    }
    *owns_session = true;
    let stop = process::stopped()?;
    let checks = service.checks()?;
    require(
        !checks.is_empty(),
        "Configure at least one real check: check-set build --command 'make check'",
    )?;
    let mut notified = false;
    // Only a run the owner started skips a provider-limit wait; continuations honour it.
    let mut owner_start = !options.continue_session;
    loop {
        if service
            .execution_session()?
            .is_some_and(|s| s.state == "paused")
        {
            return Ok("Paused; no later task started".into());
        }
        if stop.load(Ordering::Relaxed) {
            return Ok("Runner stopped; no next task started".into());
        }
        if std::mem::take(&mut owner_start) {
            service.end_limit_wait()?;
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
        connected(&task.definition().definition)?;
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
        if options.queue {
            service.supervisor_continuing_queue()?;
        }
        if options.notify && !notified {
            println!("ready");
            std::io::stdout().flush()?;
        }
        notified = true;
        let mut context = Context {
            paths,
            workspace,
            stop: Arc::clone(&stop),
            cursor,
            started: Instant::now(),
            prior_ms: run.active_ms,
            earlier_task_ms: service.earlier_task_active_ms(&run)?,
        };
        // Only an attempt that directly follows a failure is a repair; a judge-replanned
        // attempt ends as interrupted, so the next one starts fresh on the new plan.
        let repair = task.attempts.last().is_some_and(|a| {
            a.state == "failed"
                || (matches!(a.state.as_str(), "paused" | "waiting")
                    && a.steps.iter().any(|step| step.role == "remediation"))
        });
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
            cycle::run(service, &run, &mut context, &checks, repair)
        });
        let outcome = match result {
            // Until a turn succeeds after a provider limit, odd provider errors are
            // part of that outage (Codex fails compaction while out of quota).
            Ok(Outcome::Failed("transport", _)) if service.rate_waits(&run)? > 0 => {
                Outcome::RateLimit(None)
            }
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
            Outcome::Passed(summary) => match workspace.checkpoint(
                run.task,
                run.attempt,
                &base,
                &service
                    .progress(&run)?
                    .ok_or_else(|| Error::Conflict("Missing verified workflow state".into()))?
                    .verified_tree,
            ) {
                Ok(head) => {
                    context.persist_active_time(service, &run)?;
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
        // Include orchestration and operational backoff, even when no worker launched.
        // Freeze the clock before handing ownership to a pause, wait, or later attempt.
        context.persist_active_time(service, &run)?;
        match outcome {
            // Never fail a task for provider limits: wait for the provider's own reset
            // time when it names one, else back off, then resume the same attempt.
            Outcome::RateLimit(reset_at) => {
                service.rate_wait(&run, reset_at)?;
                if options.queue && wait_for_backoff(service, &run, &context)? {
                    continue;
                }
                service.release_supervisor(&run)?;
                return Ok(
                    "Waiting for scheduled rate-limit backoff; resume the same attempt when eligible"
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
                if service.automatic_replans(run.task)? >= 10 {
                    let reason = format!(
                        "Automatic recovery exhausted after 10 judge replans; latest judgment: {message}. Work and evidence retained. Edit current or apply manual remediation to continue."
                    );
                    service.finish_run(&run, "failed", Some("protocol"), &reason, None)?;
                    return Err(Error::Conflict(reason));
                }
                service.finish_run(&run, "interrupted", Some("interrupted"), &message, None)?;
                service.record_automatic_replan(run.task)?;
                apply_judgment(service, &run, paths, workspace, judgment)?;
                service.replan_applied(&run)?;
                if stop.load(Ordering::Relaxed) {
                    return Ok(message);
                }
                continue;
            }
            // A failed attempt is repaired automatically: the next attempt opens with the
            // read-only failure judge. Only a failure no repair can change stops execution.
            Outcome::Failed(category, message) => {
                service.finish_run(&run, "failed", Some(category), &message, None)?;
                let task = service
                    .task(run.task)?
                    .ok_or_else(|| Error::Conflict("Current task disappeared".into()))?;
                if let Some(stop) = crate::application::stop::unresolvable(&task) {
                    return Err(Error::Conflict(format!(
                        "Task #{} stopped ({category}): {} {}",
                        run.task, stop.reason, stop.unresolved
                    )));
                }
                service.output(
                    &run,
                    "system",
                    &format!(
                        "Attempt failed ({category}); starting a repair attempt automatically"
                    ),
                )?;
                if stop.load(Ordering::Relaxed) {
                    return Ok("Runner stopped; repair attempt not started".into());
                }
                continue;
            }
            Outcome::Passed(_) | Outcome::Changes { .. } => unreachable!(),
        }
    }
}

pub fn supervisor_alive(service: &Service) -> Result<bool> {
    Workspace::supervisor_alive(&service.project_root()?)
}

/// Called after a committed answer, approval or Human acknowledgment. The
/// conditional state change elects one window to launch the next supervisor.
pub fn continue_after_intervention(service: &mut Service, paths: &Paths) -> Result<bool> {
    continue_session(service, paths, true)
}

/// The exiting supervisor may claim an intervention already recorded by
/// another process, but must never create a new intervention itself.
fn continue_pending_session(service: &mut Service, paths: &Paths) -> Result<bool> {
    continue_session(service, paths, false)
}

fn continue_session(service: &mut Service, paths: &Paths, intervention: bool) -> Result<bool> {
    let result = continue_session_inner(service, paths, intervention);
    if let Err(error) = &result {
        service.continuation_error(&format!("Continuation could not be confirmed: {error}"))?;
    }
    result
}

fn continue_session_inner(
    service: &mut Service,
    paths: &Paths,
    intervention: bool,
) -> Result<bool> {
    let Some(session) = service.execution_session()? else {
        return Ok(false);
    };
    if session.mode == "one" {
        if intervention {
            service.single_intervention_resolved()?;
        }
        return Ok(false);
    }
    if session.mode != "queue"
        || if intervention {
            !matches!(
                session.state.as_str(),
                "running" | "waiting" | "resume_requested"
            )
        } else {
            session.state != "resume_requested"
        }
    {
        return Ok(false);
    }
    let Some(task) = service.current_task()? else {
        if intervention {
            service.complete_execution_session_if_empty()?;
        }
        return Ok(false);
    };
    if task.definition().definition.kind != crate::application::Kind::Ai
        || task.definition().lifecycle != "ready"
        || !matches!(
            task.status.as_str(),
            "ready" | "failed" | "waiting" | "paused"
        )
        || service.open_question(task.id)?.is_some()
    {
        return Ok(false);
    }
    if intervention {
        service.request_execution_continuation()?;
    }
    if supervisor_alive(service)? {
        return Ok(false);
    }
    if matches!(task.status.as_str(), "waiting" | "paused")
        && let Some(attempt) = task.attempts.last()
        && !service.supervisor_released(attempt.id)?
    {
        // A missing release event still requires explicit recovery, even after
        // an answer arrives from another window.
        return Ok(false);
    }
    if !service.resume_execution_session()? {
        return Ok(false);
    }
    if let Err(error) = background(service, paths, true, true) {
        if !supervisor_alive(service)? {
            service
                .continuation_launch_failed(&format!("Continuation could not start: {error}"))?;
        }
        return Err(error);
    }
    Ok(true)
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
fn ai(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    role: &'static str,
    evidence: &str,
) -> Result<Outcome> {
    for retry in 0..=2 {
        let outcome = ai_turn(service, run, ctx, role, evidence, None)?;
        if !matches!(
            outcome,
            Outcome::Failed("transport" | "protocol" | "environment", _)
        ) || retry == 2
        {
            return Ok(outcome);
        }
        service.output(
            run,
            "system",
            &format!(
                "Provider turn failed · automatic retry {}/2 after {}s · same attempt, work retained; Pause cancels",
                retry + 1,
                1 << retry
            ),
        )?;
        if let Some(outcome) = operational_backoff(service, run, ctx, retry)? {
            return Ok(outcome);
        }
    }
    unreachable!("bounded provider retries")
}
fn operational_backoff(
    service: &Service,
    run: &Run,
    ctx: &Context<'_>,
    retry: u32,
) -> Result<Option<Outcome>> {
    let until = Instant::now() + Duration::from_secs(1 << retry);
    loop {
        if ctx.stop.load(Ordering::Relaxed) || service.control_cursor(run)? > ctx.cursor {
            ctx.persist_active_time(service, run)?;
            return Ok(Some(Outcome::Paused));
        }
        if let Some(outcome) = ctx.deadline_outcome(run) {
            ctx.persist_active_time(service, run)?;
            return Ok(Some(outcome));
        }
        if Instant::now() >= until {
            ctx.persist_active_time(service, run)?;
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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
    let provider = Provider::of(model)?;
    let step = service.start_ai_step(run, role, model, &provider.auth_scope()?, nudge.is_some())?;
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
    let prompt = nudge.map_or_else(
        || {
            agent::prompt(
                run,
                role,
                evidence,
                &ctx.paths.executable,
                &ctx.workspace.root,
            )
        },
        |question| agent::nudge(role, question),
    );
    let launch = provider.launch(
        model,
        step.provider_session.as_deref(),
        Access::for_role(role),
        &agent::schema(),
        prompt,
    );
    let invocation = Invocation {
        executable: provider.executable()?,
        args: launch.args,
        env: launch.env,
        cwd: ctx.workspace.root.clone(),
        prompt: launch.prompt,
        schema: launch.schema_file,
        scratch_root: ctx.paths.cache.join("workers"),
        worker_lock: ctx.workspace.worker_lock.clone(),
        evidence_pin_guardian: None,
        callback: matches!(role, "execution" | "remediation").then(|| callback::Capability {
            project: ctx.workspace.root.clone(),
            state: ctx.paths.state.clone(),
            task: run.task,
            attempt: run.attempt,
            token: run.token.clone(),
        }),
    };
    let mut stream = provider::Stream::new(provider);
    match execute(
        service,
        run,
        &step,
        ctx,
        &invocation,
        run.definition.stage_timeout_ms,
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
    let mut assessment = match agent::assessment(&stream, run.definition.criteria.len()) {
        Ok(a) => a,
        Err(e) => {
            service.finish_step(&step, false, &e.to_string(), None)?;
            return Ok(Outcome::Failed("protocol", e.to_string()));
        }
    };
    if role == "validation" {
        for reason in service.check_regressions(run)? {
            assessment.findings.push(agent::Finding {
                severity: agent::Severity::Critical,
                description: reason,
            });
        }
        if !assessment.findings.is_empty()
            && assessment
                .findings
                .iter()
                .any(|finding| finding.severity == agent::Severity::Critical)
        {
            assessment.outcome = "changes_requested".into();
        }
    }
    if assessment.outcome == "blocked" {
        service.finish_assessment(&step, false, &assessment.summary)?;
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
    let critical = assessment.criteria.iter().any(|c| !c.passed)
        || assessment
            .findings
            .iter()
            .any(|f| f.severity == agent::Severity::Critical)
        || assessment.outcome == "failed";
    let needs_work =
        critical || !assessment.findings.is_empty() || assessment.outcome == "changes_requested";
    for criterion in &assessment.criteria {
        service.criterion(
            run,
            &step,
            criterion.ordinal,
            criterion.passed,
            &criterion.evidence,
        )?;
    }
    let mut summary = assessment.summary.clone();
    for finding in &assessment.findings {
        let severity = if finding.severity == agent::Severity::Critical {
            "Critical"
        } else {
            "Minor"
        };
        summary.push_str(&format!("\n{severity}: {}", finding.description));
    }
    service.finish_assessment(&step, needs_work, &summary)?;
    let report = serde_json::json!({
        "task_id": run.task, "attempt_id": run.attempt,
        "step_id": step.id, "role": role, "assessment": assessment,
    })
    .to_string();
    Ok(if needs_work {
        Outcome::Changes { report, critical }
    } else {
        Outcome::Passed(report)
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
    for retry in 0..=2 {
        let outcome = judge_once(service, run, ctx, failed)?;
        if !matches!(
            outcome,
            Outcome::Failed("transport" | "protocol" | "environment", _)
        ) || retry == 2
        {
            return Ok(outcome);
        }
        service.output(run, "system", &format!(
            "Failure judge failed · automatic retry {}/2 after {}s · same attempt, evidence retained; Pause cancels",
            retry + 1, 1 << retry,
        ))?;
        if let Some(outcome) = operational_backoff(service, run, ctx, retry)? {
            return Ok(outcome);
        }
    }
    unreachable!("bounded judge retries")
}
fn judge_once(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    failed: i64,
) -> Result<Outcome> {
    let model = run.definition.models[3].as_ref().expect("preflight models");
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
    let mut prompt = agent::judge_prompt(run, &evidence);
    let mut schema = agent::judge_schema();
    if normal_cycle {
        prompt.push_str("\nThis is a normal review/fix round within the same active task, not a failed task. Revalidate the current findings; preserve the goal and useful work. Prefer repair. You may revise the task or insert prerequisites only if needed, retaining the full goal and acceptance bar. The repair counter follows the original task across automatic revisions and prerequisite handoffs. Do not ask the owner or escalate models during these rounds; unresolved critical findings are escalated only after ten repair rounds.");
        schema["properties"]["judgment"]["anyOf"]
            .as_array_mut()
            .expect("judge alternatives")
            .truncate(3);
    }
    let launch = provider.launch(
        model,
        step.provider_session.as_deref(),
        Access::ReadOnly,
        &schema,
        prompt,
    );
    let invocation = Invocation {
        executable: provider.executable()?,
        args: launch.args,
        env: launch.env,
        cwd: ctx.workspace.root.clone(),
        prompt: launch.prompt,
        schema: launch.schema_file,
        scratch_root: ctx.paths.cache.join("workers"),
        worker_lock: ctx.workspace.worker_lock.clone(),
        evidence_pin_guardian: None,
        callback: None,
    };
    let mut stream = provider::Stream::new(provider);
    match execute(
        service,
        run,
        &step,
        ctx,
        &invocation,
        run.definition.stage_timeout_ms,
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
    let judgment = match agent::parse_judgment(stream.last_message.as_deref().unwrap_or("")) {
        Ok(j) => j,
        Err(e) => {
            service.finish_step(&step, false, &e.to_string(), None)?;
            return Ok(Outcome::Failed("protocol", e.to_string()));
        }
    };
    if normal_cycle
        && !matches!(
            judgment,
            agent::Judgment::Repair { .. }
                | agent::Judgment::Revise { .. }
                | agent::Judgment::Insert { .. }
        )
    {
        let message = "Fix planner must repair, revise or insert during the normal ten-round cycle; owner escalation waits for the final critical decision";
        service.finish_step(&step, false, message, None)?;
        return Ok(Outcome::Failed("protocol", message.into()));
    }
    let (summary, outcome) = match judgment {
        agent::Judgment::Repair { plan, reason } => (
            format!("Repair: {reason}\nAuthorized repair plan:\n{plan}"),
            Outcome::Passed(plan),
        ),
        agent::Judgment::Ask { question, reason } => {
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

fn describe(judgment: &agent::Judgment) -> String {
    match judgment {
        agent::Judgment::Repair { reason, .. } => format!("Repair: {reason}"),
        agent::Judgment::Revise { reason, .. } => {
            format!("Task revised by the failure judge: {reason}")
        }
        agent::Judgment::Insert { tasks, reason } => format!(
            "{} prerequisite task(s) inserted by the failure judge: {reason}",
            tasks.len()
        ),
        agent::Judgment::Escalate { model, reason } => format!(
            "Execution escalated to {}/{} by the failure judge: {reason}",
            model.provider, model.model
        ),
        agent::Judgment::Ask { reason, .. } => format!("Owner decision needed: {reason}"),
    }
}

/// Applied once the judged attempt has ended, so ordinary revision and insertion rules hold.
fn apply_judgment(
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
    let ready = service.preferences(&paths.config).is_ok_and(|p| {
        p.planner_approval != crate::application::preferences::PlannerApproval::ReviewRequired
    });
    let judge = crate::application::Actor {
        kind: crate::application::ActorKind::Ai,
        name: "judge".into(),
    };
    match judgment {
        agent::Judgment::Revise {
            title,
            goal,
            criteria,
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
            service.revise_as(target, &definition, ready, &judge, Some(&reason))?;
        }
        agent::Judgment::Insert { tasks, reason } => {
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
                    stage_timeout_ms: run.definition.stage_timeout_ms,
                })
                .collect();
            remediation::preserve(service, workspace, target)?;
            service.apply_remediation(target, None, &repairs, &reason, ready)?;
        }
        agent::Judgment::Escalate { model, reason } => {
            service.apply_remediation(target, Some(model), &[], &reason, true)?;
        }
        agent::Judgment::Repair { .. } | agent::Judgment::Ask { .. } => {}
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
    stream: &mut provider::Stream,
) -> Result<Outcome> {
    ctx.persist_active_time(service, run)?;
    if let Some(outcome) = ctx.deadline_outcome(run) {
        ctx.persist_active_time(service, run)?;
        return Ok(outcome);
    }
    let mut worker = match Worker::spawn(&ctx.paths.executable, spec) {
        Ok(worker) => worker,
        Err(error) => {
            ctx.persist_active_time(service, run)?;
            return Ok(Outcome::Failed("environment", error.to_string()));
        }
    };
    let started = Instant::now();
    let mut interrupted = None;
    let mut last_tick = Instant::now();
    let mut stdout = output::Utf8Stream::default();
    let mut stderr = output::Utf8Stream::default();
    loop {
        if interrupted.is_none() {
            if ctx.stop.load(Ordering::Relaxed) || service.control_cursor(run)? > ctx.cursor {
                interrupted = Some(Outcome::Paused);
                worker.stop();
            } else if let Some(limit) =
                timeout.filter(|&n| started.elapsed().as_millis() >= n as u128)
            {
                interrupted = Some(Outcome::Failed(
                    "timeout",
                    if ai {
                        format!(
                            "{}: the agent ran {} without returning a result; work and transcript retained",
                            crate::application::stop::STAGE_LIMIT,
                            crate::application::limits::label(Some(limit))
                        )
                    } else {
                        "Individual supervisor check timeout reached; work and transcript retained"
                            .into()
                    },
                ));
                worker.stop();
            } else if let Some(outcome) = ctx.deadline_outcome(run) {
                interrupted = Some(outcome);
                worker.stop();
            }
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            ctx.persist_active_time(service, run)?;
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
                            provider::Event::Session(id) => service.record_session(step, &id)?,
                            provider::Event::Usage(usage, cost) => {
                                service.record_usage(step, &usage, cost)?
                            }
                            provider::Event::Text(text) => service.output(run, "stdout", &text)?,
                            provider::Event::Message(reply) => {
                                service.output(run, "stdout", &agent::display(&reply))?
                            }
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
                ctx.persist_active_time(service, run)?;
                if interrupted.is_none() && ai && stream.rate_limited {
                    return Ok(Outcome::RateLimit(stream.reset_at_ms));
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
                                if ai { stream.provider.label() } else { "Check" }
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
pub fn background(
    service: &mut Service,
    paths: &Paths,
    queue: bool,
    continue_session: bool,
) -> Result<()> {
    drop(Workspace::acquire(&service.project_root()?)?);
    require(
        !service.checks()?.is_empty(),
        "Configure an enabled check in Configuration > Checks (or CLI check-set)",
    )?;
    if let Some(task) = service.current_task()? {
        connected(&task.definition().definition)?;
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
    if continue_session {
        command.arg("--continue-session");
    }
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
