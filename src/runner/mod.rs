pub(crate) mod agent;
pub(crate) mod callback;
mod checks;
pub mod control;
mod cycle;
mod delivery;
mod failure_summary;
mod output;
pub(crate) mod process;
pub(crate) mod provider;
pub mod remediation;
mod reset;
pub(crate) mod sandbox;
pub mod setup;
mod test_summary;
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
    os::unix::fs::{MetadataExt, PermissionsExt},
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
}
enum Outcome {
    Passed(String),
    Failed(&'static str, String),
    Changes {
        report: String,
        critical: bool,
    },
    Paused,
    DeliveryBlocked(String),
    Question(String),
    /// Provider rate or usage limit, with its own reset time when it names one.
    RateLimit(Option<i64>),
    /// The failure judge changed the plan (task revised, work inserted, model escalated);
    /// this attempt ends without spending budget, the plan is applied, and the loop goes on.
    Replanned(agent::Judgment),
}

fn settle_result(result: Result<Outcome>, had_rate_wait: bool) -> Outcome {
    match result {
        Ok(Outcome::Failed("transport", _)) if had_rate_wait => Outcome::RateLimit(None),
        Ok(outcome) => outcome,
        Err(Error::Conflict(message)) if message.starts_with("Resource limit:") => {
            Outcome::DeliveryBlocked(message)
        }
        Err(error) => Outcome::Failed("environment", error.to_string()),
    }
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
                "Active time limit reached (configured task-wide limit); work and evidence retained. Retry restarts the limit".into(),
            ))
    }
}
/// Every role names a supported provider whose CLI is installed.
/// Everything a task's definition needs before a supervisor can run it.
fn preflight(service: &Service, definition: &crate::application::Definition) -> Result<()> {
    service.validate_definition_models(definition)?;
    for (index, (role, model)) in crate::application::ROLES
        .iter()
        .zip(&definition.models)
        .enumerate()
    {
        let model = model.as_ref().ok_or_else(|| {
            Error::Conflict(format!(
                "Configure all four task roles in Models ({}); defaults only apply to new tasks",
                provider::SUPPORTED
            ))
        })?;
        model.validate_for_role(role)?;
        let provider = Provider::of(model)?;
        provider.executable()?;
        if index == 0 {
            provider.writable_network(service.agent_network()?)?;
        }
    }
    sandbox::available()
}

/// The worker for one provider turn in `project`. A read-only turn runs in the sandbox, so
/// whatever its provider allows, it cannot change the project or ktask's own records.
pub(crate) fn provider_invocation(
    provider: Provider,
    launch: provider::Launch,
    access: Access,
    project: &std::path::Path,
    paths: &Paths,
    scratch_root: PathBuf,
    worker_lock: PathBuf,
) -> Result<Invocation> {
    Ok(Invocation {
        executable: provider.executable()?,
        args: launch.args,
        env: launch.env,
        cwd: project.to_owned(),
        prompt: launch.prompt,
        schema: launch.schema_file,
        scratch_root,
        artifact_target: None,
        worker_lock,
        callback: None,
        sandbox: match access {
            Access::ReadOnly => Some(sandbox::Sandbox::for_project(project, paths)?),
            Access::Write => None,
        },
    })
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
    let workspace = Workspace::acquire(&service.project_root()?)?;
    // Holding the lease proves no supervisor owns the project: a running attempt is orphaned.
    service.settle_orphan()?;
    control::finish_pending_with_workspace(service, &workspace)?;
    workspace::git_identity(&workspace.root)?;
    let mut owns_session = false;
    let result = run_inner(service, paths, options, &workspace, &mut owns_session);
    if !owns_session {
        return result;
    }
    let mut requested = false;
    let session = service.execution_session()?;
    if session.as_ref().is_some_and(|s| s.state == "paused") {
        service.pause_settled()?;
    }
    if let Some(session) = session
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
    // The control is owned by this supervisor, not the terminal that requested it.
    control::finish_pending_with_workspace(service, &workspace)?;
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

/// Why the queue stops before `task`, or None when it can run: Human work and approvals
/// wait for the owner, and only a state that moves forward by running is run, or this
/// runner's own provider-limit wait once the limit lifts.
fn admit(
    service: &Service,
    task: &crate::application::Task,
    notified: bool,
) -> Result<Option<String>> {
    use crate::application::lifecycle::{Action, State};
    let human = task.definition().definition.kind == crate::application::Kind::Human;
    if notified && (human || task.definition().lifecycle != "ready") {
        return Ok(Some("Queue stopped for human work or approval".into()));
    }
    require(
        !human,
        "A Human task is done when you mark it done; there is nothing to run",
    )?;
    let state = task.state();
    if state == State::NeedsApproval {
        return Ok(Some(format!("Stopped: task #{} needs approval", task.id)));
    }
    let runs = matches!(
        state.forward(),
        Some(Action::Run | Action::Resume | Action::Retry)
    ) || (state == State::Running && task.status == "waiting");
    require(
        runs || (state == State::NeedsInput && service.open_question(task.id)?.is_none()),
        &format!(
            "Task #{} is {}; nothing to run",
            task.id,
            state.label().to_lowercase()
        ),
    )?;
    Ok(None)
}

/// Whether the task's last attempt failed because the checks were red before it started.
fn red_start(task: &crate::application::Task) -> bool {
    task.attempts.last().is_some_and(|a| {
        a.outcome
            .as_deref()
            .is_some_and(|o| o.starts_with(crate::application::stop::BASELINE_RED))
    })
}

/// A task edited while paused or waiting for an answer continues in a new attempt that uses
/// the new revision, as Retry does. True when that happened.
fn adopt_revision(
    service: &mut Service,
    task: crate::application::Task,
) -> Result<(crate::application::Task, bool)> {
    let revised = matches!(task.status.as_str(), "paused" | "waiting")
        && task
            .attempts
            .last()
            .is_some_and(|a| a.revision != task.definition().number);
    if !revised {
        return Ok((task, false));
    }
    service.end_for_new_revision(VersionedTask {
        id: task.id,
        version: task.version,
    })?;
    let task = service
        .current_task()?
        .ok_or_else(|| Error::Conflict("Current task disappeared".into()))?;
    Ok((task, true))
}

fn run_inner(
    service: &mut Service,
    paths: &Paths,
    options: &Options,
    workspace: &Workspace,
    owns_session: &mut bool,
) -> Result<String> {
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
    // Set by a failure: the next attempt is the supervisor's own repair. A replanned task's
    // is too, whenever it is current again (after any inserted prerequisites).
    let mut repairing = false;
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
        if let Some(stopped) = admit(service, &task, notified)? {
            return Ok(stopped);
        }
        let (task, revised) = adopt_revision(service, task)?;
        let opening = if std::mem::take(&mut repairing) || service.replan_pending(task.id)? {
            cycle::Opening::Repair
        } else if revised || (task.status == "failed" && !red_start(&task)) {
            cycle::Opening::Retry
        } else {
            // A task's own run, or a retry after a red start, begins from a verified green
            // state. Inserted prerequisite tasks exist to repair, so they start at once.
            cycle::Opening::Fresh {
                verify: task.origin_task_id.is_none()
                    && service.check_before_task()?
                    && workspace.clean()?,
            }
        };
        preflight(service, &task.definition().definition)?;
        let base = workspace.head()?;
        require(
            workspace.clean()?
                || service.pending_delivery_handoff(task.id)?
                || workspace::retains_work_from(
                    &workspace.root,
                    service.attempt_base(task.id)?.as_deref(),
                    &base,
                )?
                || service
                    .current_delivery_stage(task.id)?
                    .is_some_and(|s| s != "work"),
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
            opening != cycle::Opening::Repair,
        )?;
        // A live attempt blocks configuration changes before settings are read or Git mutates.
        let delivery_init: Result<()> = (|| {
            if service.attempt_delivery(&run).is_ok() {
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
            if service.claim_delivery_handoff(&run, &config, branch.as_deref(), &destination)? {
                return Ok(());
            }
            service.ensure_attempt_delivery(&run, &config, branch.as_deref(), &destination)?;
            Ok(())
        })();
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

        let result = (|| -> Result<Outcome> {
            service.output(
                &run,
                "system",
                match opening {
                    cycle::Opening::Repair => {
                        "Repair started with the configured remediation model"
                    }
                    cycle::Opening::Retry => "Retry started from the kept work; checks run first",
                    cycle::Opening::Fresh { .. } => {
                        "Execution started; results require checks, review and validation"
                    }
                },
            )?;
            let mut delivery_retries = 0_u32;
            loop {
                if let Err(error) = &delivery_init {
                    break Ok(Outcome::DeliveryBlocked(error.to_string()));
                }
                match delivery::prepare(service, &run, &context) {
                    Ok(false) => break Ok(Outcome::Paused),
                    Ok(true) => {}
                    Err(error) => {
                        if let Some(outcome) =
                            delivery::retry(service, &run, &context, &error, &mut delivery_retries)?
                        {
                            break Ok(outcome);
                        }
                        continue;
                    }
                }
                if let Err(error) = delivery::reconcile_resume(service, &run, &context) {
                    break Ok(Outcome::DeliveryBlocked(error.to_string()));
                }
                let result = service.checks().and_then(|checks| {
                    require(
                        !checks.is_empty(),
                        "Configure at least one enabled project check",
                    )?;
                    cycle::run(service, &run, &mut context, &checks, opening)
                });
                let result = match result {
                    Ok(Outcome::Failed("transport" | "environment" | "protocol", message))
                        if service.attempt_delivery(&run)?.stage != "work" =>
                    {
                        Ok(Outcome::DeliveryBlocked(message))
                    }
                    Err(error) if service.attempt_delivery(&run)?.stage != "work" => {
                        Ok(Outcome::DeliveryBlocked(error.to_string()))
                    }
                    other => other,
                };
                let Ok(Outcome::Passed(summary)) = result else {
                    break result;
                };
                if stop.load(Ordering::Relaxed) || service.control_cursor(&run)? > context.cursor {
                    break Ok(Outcome::Paused);
                }
                let verified = service
                    .progress(&run)?
                    .ok_or_else(|| Error::Conflict("Missing verified workflow state".into()))?
                    .verified_tree;
                let delivery_state = service.attempt_delivery(&run)?;
                let expected_head = delivery_state.checkpoint_head.clone().unwrap_or(
                    service
                        .attempt_base(run.task)?
                        .ok_or_else(|| Error::Conflict("Attempt baseline missing".into()))?,
                );
                service.delivery_stage(&run, "checkpointing", Some(&expected_head), None, None)?;
                let head = match workspace.checkpoint(
                    run.task,
                    run.attempt,
                    &expected_head,
                    &verified,
                    matches!(
                        delivery_state.stage.as_str(),
                        "checkpointed" | "integrating" | "verified" | "pushed" | "pr_ready"
                    ),
                ) {
                    Ok(head) => head,
                    Err(error) => break Ok(Outcome::DeliveryBlocked(error.to_string())),
                };
                service.delivery_stage(&run, "checkpointed", Some(&head), None, None)?;
                match delivery::publish(service, &run, &mut context, &summary, &head) {
                    Ok(delivery::DeliveryOutcome::Done(report)) => {
                        context.persist_active_time(service, &run)?;
                        service.finish_run(
                            &run,
                            "succeeded",
                            None,
                            &report,
                            Some(&workspace.head()?),
                        )?;
                        break Ok(Outcome::Passed(report));
                    }
                    Ok(delivery::DeliveryOutcome::Retry) => {
                        delivery_retries = 0;
                        continue;
                    }
                    Ok(delivery::DeliveryOutcome::Paused) => break Ok(Outcome::Paused),
                    Ok(delivery::DeliveryOutcome::Exhausted(reason)) => {
                        break Ok(cycle::escalation(&reason));
                    }
                    Err(error) => {
                        if let Some(outcome) =
                            delivery::retry(service, &run, &context, &error, &mut delivery_retries)?
                        {
                            break Ok(outcome);
                        }
                        continue;
                    }
                }
            }
        })();
        // A transport error during a provider wait belongs to the same outage.
        let had_rate_wait =
            matches!(&result, Ok(Outcome::Failed("transport", _))) && service.rate_waits(&run)? > 0;
        let outcome = settle_result(result, had_rate_wait);
        if !options.queue
            && let Outcome::Passed(report) = &outcome
        {
            return Ok(report.clone());
        }
        let outcome = if matches!(outcome, Outcome::Passed(_))
            && (stop.load(Ordering::Relaxed) || service.control_cursor(&run)? > context.cursor)
        {
            Outcome::Paused
        } else {
            outcome
        };
        if matches!(outcome, Outcome::Passed(_)) {
            continue;
        }
        // Include orchestration and operational backoff, even when no worker launched.
        // Freeze the clock before handing ownership to a pause, wait, or later attempt.
        context.persist_active_time(service, &run)?;
        match outcome {
            // Never fail a task for provider limits: wait for the provider's own reset
            // time when it names one, else back off, then resume the same attempt.
            Outcome::RateLimit(reset_at) => {
                service.rate_wait(&run, reset_at)?;
                match wait_for_backoff(service, &run, &context) {
                    Ok(true) => continue,
                    Err(Error::Conflict(reason)) if reason.starts_with("Resource limit:") => {
                        service.pause_wait(&run, &reason)?;
                        service.release_supervisor(&run)?;
                        return Ok(format!("Paused: {reason}"));
                    }
                    Err(error) => return Err(error),
                    Ok(false) => {}
                }
                // Paused during the wait: Resume tries again at once.
                service.pause_wait(
                    &run,
                    "Paused during a provider limit wait; Resume tries again now",
                )?;
                service.release_supervisor(&run)?;
                return Ok("Paused during a provider limit wait".into());
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
            Outcome::DeliveryBlocked(reason) => {
                service.finish_run(&run, "paused", None, &reason, None)?;
                service.release_supervisor(&run)?;
                return Ok(format!("Paused: {reason}"));
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
                if let Err(error) = service.output(
                    &run,
                    "system",
                    &format!(
                        "Attempt failed ({category}); starting a repair attempt automatically"
                    ),
                ) {
                    if let Error::Conflict(reason) = &error
                        && reason.starts_with("Resource limit:")
                    {
                        service.finish_run(&run, "paused", None, reason, None)?;
                        service.release_supervisor(&run)?;
                        return Ok(format!("Paused: {reason}"));
                    }
                    return Err(error);
                }
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
                if stop.load(Ordering::Relaxed) {
                    return Ok("Runner stopped; repair attempt not started".into());
                }
                repairing = true;
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
        if !intervention {
            return Ok(false);
        }
        service.single_intervention_resolved()?;
        return resume_answered(service, paths);
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

/// Run one: an answer is the owner's go-ahead for the same task, so its attempt continues
/// at once. Run one still never moves on to another task.
fn resume_answered(service: &mut Service, paths: &Paths) -> Result<bool> {
    let Some(task) = service.current_task()? else {
        return Ok(false);
    };
    if task.state() != crate::application::lifecycle::State::NeedsInput
        || service.open_question(task.id)?.is_some()
        || supervisor_alive(service)?
    {
        return Ok(false);
    }
    background(service, paths, false, false)?;
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
    let configured = agent::model_role(role);
    let index = crate::application::ROLES
        .iter()
        .position(|r| *r == configured)
        .expect("role");
    let model = run.definition.models[index]
        .as_ref()
        .expect("preflight models");
    let provider = Provider::of(model)?;
    let step = service.start_ai_step(
        run,
        configured,
        model,
        &provider.auth_scope()?,
        nudge.is_some(),
    )?;
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
    let access = Access::for_role(configured);
    let network = access == Access::Write && service.agent_network()?;
    if access == Access::Write {
        provider.writable_network(network)?;
    }
    let launch = provider.launch(
        model,
        step.provider_session.as_deref(),
        access,
        network,
        &agent::schema(),
        prompt,
    );
    let mut invocation = provider_invocation(
        provider,
        launch,
        access,
        &ctx.workspace.root,
        ctx.paths,
        ctx.paths.cache.join("workers"),
        ctx.workspace.worker_lock.clone(),
    )?;
    invocation.callback = (access == Access::Write).then(|| callback::Capability {
        project: ctx.workspace.root.clone(),
        state: ctx.paths.state.clone(),
        task: run.task,
        attempt: run.attempt,
        token: run.token.clone(),
    });
    let mut stream = provider::Stream::new(provider);
    if let Some(outcome) = provider_turn(
        service,
        run,
        &step,
        ctx,
        &invocation,
        &mut stream,
        configured,
    )? {
        return Ok(outcome);
    }
    let mut assessment = match agent::assessment(&stream, run.definition.criteria.len()) {
        Ok(a) => a,
        Err(e) => {
            service.finish_step(&step, false, &e.to_string())?;
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
    // Minor findings are recorded with the accepted result; only critical ones cost a round.
    // A worker's self-assessment never does: the supervisor's checks and the independent
    // judges are the gate, and the worker leaves the full check to the supervisor.
    let needs_work = critical && !matches!(role, "execution" | "repair");
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
/// One provider process for an AI step, under the stage limit. None means the provider
/// exited cleanly and `stream` holds its reply; otherwise the outcome says why not, and a
/// failed step is already recorded. Pause and limit waits leave the step to the attempt.
fn provider_turn(
    service: &mut Service,
    run: &Run,
    step: &Step,
    ctx: &mut Context<'_>,
    spec: &Invocation,
    stream: &mut provider::Stream,
    role: &str,
) -> Result<Option<Outcome>> {
    let timeout = run.definition.stage_timeout_ms;
    let outcome = match execute(
        service,
        run,
        step,
        ctx,
        spec,
        timeout,
        Some((&mut *stream, role)),
    )? {
        Finish::Exited(_) if stream.rate_limited => Outcome::RateLimit(stream.reset_at_ms),
        Finish::Exited(0) if stream.configuration_error.is_none() => return Ok(None),
        Finish::Exited(code) => match &stream.configuration_error {
            Some((setting, detail)) => Outcome::Failed(
                "configuration",
                format!(
                    "{role} {setting} rejected by {} for {} (exit {code}): {detail}. Fix the {setting} setting before retrying",
                    stream.provider.label(),
                    crate::application::ROLES
                        .iter()
                        .position(|candidate| *candidate == role)
                        .and_then(|index| run.definition.models[index].as_ref())
                        .map(|model| format!("{}/{}", model.provider, model.model))
                        .unwrap_or_else(|| "configured model".into()),
                ),
            ),
            None => Outcome::Failed(
                "transport",
                match stream.error_summary() {
                    Some(error) => format!("{} exited {code}: {error}", stream.provider.label()),
                    None => format!(
                        "{} exited {code} without an error message; see stored output",
                        stream.provider.label()
                    ),
                },
            ),
        },
        Finish::Stopped(outcome) => outcome,
    };
    if let Outcome::Failed(_, message) = &outcome {
        service.finish_step(step, false, message)?;
    }
    Ok(Some(outcome))
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
        prompt.push_str("\nThis is a normal review/fix round within the same active task, not a failed task. Revalidate the current findings; preserve the goal and useful work. Plan fixes only for critical findings and drop minor ones. Cost to result matters most, then time: prefer repair while the worker is converging, but escalate to a stronger execution model or effort when the evidence shows it is not (for example a critical finding it already failed to fix, or a defect beyond its reach). You may revise the task or insert prerequisites only if needed, retaining the full goal and acceptance bar. The repair counter follows the original task across automatic revisions, escalations and prerequisite handoffs. Do not ask the owner during these rounds.");
        schema["properties"]["judgment"]["anyOf"]
            .as_array_mut()
            .expect("judge alternatives")
            .retain(|choice| choice["properties"]["action"]["enum"][0] != "ask");
    }
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
        ctx.paths.cache.join("workers"),
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
        let message = "Fix planner must repair, revise, insert or escalate during the normal ten-round cycle; owner questions wait for the final critical decision";
        service.finish_step(&step, false, message)?;
        return Ok(Outcome::Failed("protocol", message.into()));
    }
    let (summary, outcome) = match judgment {
        // The first line is the judge's reason, shown to the owner as the round's why.
        agent::Judgment::Repair { plan, reason } => (
            format!("Repair: {reason}\nAuthorized repair plan:\n{plan}"),
            Outcome::Passed(format!("{}\n{plan}", reason.replace('\n', " "))),
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
            if service.attempt_delivery(run)?.stage == "work" {
                remediation::preserve(service, workspace, target)?;
            }
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
/// How a worker process ended.
enum Finish {
    /// It exited by itself with this code.
    Exited(i32),
    /// The supervisor ended it first, or could not start it: pause, a time limit, a launch
    /// failure.
    Stopped(Outcome),
}

/// Runs one worker to its end, streaming its output into the attempt. An AI turn passes its
/// provider stream; a check passes none and its output is stored as written. Callers record
/// the step's result.
fn execute(
    service: &mut Service,
    run: &Run,
    step: &Step,
    ctx: &mut Context<'_>,
    spec: &Invocation,
    timeout: Option<i64>,
    mut stream: Option<(&mut provider::Stream, &str)>,
) -> Result<Finish> {
    let ai = stream.is_some();
    // Codex reports a resumed session's running totals, not the turn's own usage.
    let session_totals = stream
        .as_ref()
        .is_some_and(|(s, _)| s.provider == provider::Provider::Codex);
    ctx.persist_active_time(service, run)?;
    if let Some(outcome) = ctx.deadline_outcome(run) {
        ctx.persist_active_time(service, run)?;
        return Ok(Finish::Stopped(outcome));
    }
    let mut spec = spec.clone();
    let artifact_relative = format!("evidence/attempt-{}/step-{}", run.attempt, step.id);
    spec.artifact_target = Some(process::ArtifactTarget {
        state: ctx.paths.state.clone(),
        project: ctx.workspace.root.clone(),
        scratch_root: ctx.paths.cache.join("workers"),
        attempt: run.attempt,
        token: run.token.clone(),
        relative_prefix: artifact_relative,
    });
    let mut worker = match Worker::spawn(&ctx.paths.executable, &spec) {
        Ok(worker) => worker,
        Err(error) => {
            ctx.persist_active_time(service, run)?;
            return Ok(Finish::Stopped(Outcome::Failed(
                "environment",
                error.to_string(),
            )));
        }
    };
    let started = Instant::now();
    let mut interrupted = None;
    let mut last_tick = Instant::now();
    let mut stdout = output::Utf8Stream::default();
    let mut stderr = output::Utf8Stream::default();
    let mut pending = None;
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
        let message = match pending.take() {
            Some(message) => Ok(message),
            None => worker.messages.recv_timeout(Duration::from_millis(40)),
        };
        match message {
            Ok(Message::Output {
                stream: source,
                bytes,
            }) => {
                let mut rows = Vec::new();
                let mut chunk = Some((source, bytes));
                let mut chunks = 0;
                while let Some((source, bytes)) = chunk.take() {
                    chunks += 1;
                    if let Some((stream, role)) = stream.as_mut().filter(|_| source == "stdout") {
                        for event in stream.feed(&bytes)? {
                            match event {
                                provider::Event::Text(text) => rows.push(("stdout".into(), text)),
                                provider::Event::Message(reply) => {
                                    rows.push(("stdout".into(), agent::display(&reply, role)))
                                }
                                // Keep the stored order: earlier output first.
                                provider::Event::Session(id) => {
                                    service.output_rows(run, &std::mem::take(&mut rows))?;
                                    service.record_session(step, &id)?
                                }
                                provider::Event::Usage(usage, cost) => {
                                    service.output_rows(run, &std::mem::take(&mut rows))?;
                                    let usage = if session_totals {
                                        service.step_share(step, usage)?
                                    } else {
                                        usage
                                    };
                                    service.record_usage(step, &usage, cost)?
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
                            if source == "stderr"
                                && let Some((stream, _)) = stream.as_mut()
                            {
                                stream.note_error(&text);
                            }
                            rows.push((source, text));
                        }
                    }
                    // Output that queued while the database was busy joins this transaction,
                    // bounded so pause and time limits are still checked during a flood.
                    if rows.len() < 1000 && chunks < 256 {
                        match worker.messages.try_recv() {
                            Ok(Message::Output { stream, bytes }) => chunk = Some((stream, bytes)),
                            Ok(other) => pending = Some(other),
                            Err(_) => {}
                        }
                    }
                }
                service.output_rows(run, &rows)?;
            }
            Ok(Message::Exit { code }) => {
                worker.join()?;
                for (source, decoder) in [("stdout", &mut stdout), ("stderr", &mut stderr)] {
                    let text = decoder.finish();
                    if !text.is_empty() {
                        if source == "stderr"
                            && let Some((stream, _)) = stream.as_mut()
                        {
                            stream.note_error(&text);
                        }
                        service.output(run, source, &text)?;
                    }
                }
                ctx.persist_active_time(service, run)?;
                return Ok(interrupted.map_or(Finish::Exited(code), Finish::Stopped));
            }
            Ok(Message::Artifact { .. }) => {}
            Ok(Message::Error(message)) => {
                // A partial final frame is a protocol error, but the guardian's
                // exit status still carries a resource pause after cleanup.
                if let Err(Error::Conflict(reason)) = worker.join()
                    && reason.starts_with("Resource limit:")
                {
                    return Err(Error::Conflict(reason));
                }
                return Err(Error::Conflict(message));
            }
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

/// Launch the exact supervisor executable, never a PATH-selected ktask or a rebuild target.
pub fn background(
    service: &mut Service,
    paths: &Paths,
    queue: bool,
    continue_session: bool,
) -> Result<()> {
    let workspace = Workspace::acquire(&service.project_root()?)?;
    workspace::git_identity(&workspace.root)?;
    drop(workspace);
    require(
        !service.checks()?.is_empty(),
        "Configure an enabled check in Configuration > Checks (or CLI check-set)",
    )?;
    if let Some(task) = service.current_task()? {
        preflight(service, &task.definition().definition)?;
    }
    std::fs::create_dir_all(&paths.state)?;
    let log_path = paths
        .state
        .join(format!("runner-{}.log", service.project_id));
    let log_start = match std::fs::metadata(&log_path) {
        Ok(metadata) => Some((metadata.dev(), metadata.ino(), metadata.len())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut logger = Command::new(&paths.executable)
        .arg("__logger")
        .arg(&log_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let log_pipe = logger.stdin.take().expect("piped logger stdin");
    let mut command = Command::new(&paths.executable);
    command
        .arg("--project")
        .arg(service.project_root()?)
        .args(["run", "--notify"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log_pipe));
    command.arg(if queue { "--queue" } else { "--one" });
    if continue_session {
        command.arg("--continue-session");
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            drop(command);
            let _ = logger.wait();
            return Err(error.into());
        }
    };
    // Command retains its Stdio writer after spawn. Close this copy so the logger
    // sees EOF when the detached runner exits, including before startup readiness.
    drop(command);
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
        let _ = logger.wait();
        if !ready {
            // Stderr stays attached to a file so a detached runner survives its
            // launching CLI. Read bytes appended since startup, never old errors.
            let error = (|| -> std::io::Result<String> {
                let mut log = std::fs::File::open(log_path)?;
                let metadata = log.metadata()?;
                let end = metadata.len();
                let start = log_start
                    .filter(|(dev, ino, offset)| {
                        *dev == metadata.dev() && *ino == metadata.ino() && *offset <= end
                    })
                    .map_or(end.saturating_sub(8192), |(_, _, offset)| {
                        offset.max(end.saturating_sub(8192))
                    });
                log.seek(SeekFrom::Start(start))?;
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
                    .strip_prefix("conflict: ")
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

pub(crate) fn rotate_log(
    mut input: impl Read,
    path: &std::path::Path,
    limit: u64,
) -> std::io::Result<()> {
    const MARKER: &[u8] = b"[runner log rotated; earlier output is in .log.1]\n";
    let previous = path.with_extension("log.1");
    if previous.exists() {
        let mut old = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&previous)?;
        trim_log(
            &mut old,
            limit,
            b"[runner log trimmed after limit change]\n",
        )?;
    }
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    trim_log(&mut log, limit, MARKER)?;
    log.seek(SeekFrom::End(0))?;
    let mut size = log.metadata()?.len();
    let mut buffer = [0_u8; 4096];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        if size.saturating_add(n as u64) > limit {
            drop(log);
            let _ = std::fs::remove_file(&previous);
            std::fs::rename(path, &previous)?;
            log = std::fs::File::create(path)?;
            log.write_all(MARKER)?;
            size = log.metadata()?.len();
        }
        log.write_all(&buffer[..n])?;
        size += n as u64;
    }
    log.flush()
}

fn trim_log(log: &mut std::fs::File, limit: u64, marker: &[u8]) -> std::io::Result<()> {
    let size = log.metadata()?.len();
    if size <= limit {
        return Ok(());
    }
    let keep = limit.saturating_sub(marker.len() as u64);
    let mut source = size - keep;
    let mut target = marker.len() as u64;
    log.seek(SeekFrom::Start(0))?;
    log.write_all(marker)?;
    let mut buffer = [0_u8; 4096];
    while source < size {
        let n = (size - source).min(buffer.len() as u64) as usize;
        log.seek(SeekFrom::Start(source))?;
        log.read_exact(&mut buffer[..n])?;
        log.seek(SeekFrom::Start(target))?;
        log.write_all(&buffer[..n])?;
        source += n as u64;
        target += n as u64;
    }
    log.set_len(target)?;
    Ok(())
}

#[cfg(test)]
mod resource_tests {
    use super::*;
    #[test]
    fn detached_log_keeps_only_two_bounded_segments() -> std::io::Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("runner-1.log");
        std::fs::write(&path, vec![b'p'; 8190])?;
        rotate_log(std::io::Cursor::new(vec![b'x'; 100_000]), &path, 8192)?;
        let previous = path.with_extension("log.1");
        assert!(std::fs::metadata(&path)?.len() <= 8192 + 4096);
        assert!(std::fs::metadata(&previous)?.len() <= 8192 + 4096);
        assert!(std::fs::read_to_string(&path)?.contains("runner log rotated"));
        Ok(())
    }

    #[test]
    fn lowering_log_limit_compacts_existing_segments() -> std::io::Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("runner-1.log");
        let previous = path.with_extension("log.1");
        std::fs::write(&path, vec![b'a'; 30_000])?;
        std::fs::write(&previous, vec![b'b'; 30_000])?;
        rotate_log(std::io::Cursor::new(b"new"), &path, 8192)?;
        assert!(std::fs::metadata(&path)?.len() <= 8192 + 4096);
        assert!(std::fs::metadata(&previous)?.len() <= 8192 + 4096);
        assert!(std::fs::read_to_string(&path)?.ends_with("new"));
        Ok(())
    }
}
