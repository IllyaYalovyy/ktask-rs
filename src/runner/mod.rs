pub(crate) mod codex;
mod output;
pub(crate) mod process;
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
    io::{BufRead, Write},
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
}
enum Outcome {
    Passed(String),
    Failed(&'static str, String),
    Paused,
    Question(String),
    RateLimit,
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
    let workspace = Workspace::acquire_with_cleanup_confirmation(
        &service.project_root()?,
        options.recover && options.confirm_worker_cleanup,
    )?;
    if options.recover {
        if options.confirm_worker_cleanup
            && service
                .current_task()?
                .is_none_or(|t| t.status != "running")
        {
            return Ok("Worker cleanup confirmed; retry current explicitly".into());
        }
        service.recover_run()?;
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
        let repair = task.attempts.iter().any(|a| a.state == "failed");
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
                return Ok("Paused".into());
            }
            Outcome::Question(question) => {
                service.finish_run(&run, "waiting", None, &question, None)?;
                return Ok(format!("Waiting for answer: {question}"));
            }
            Outcome::Failed(category, message) => {
                service.finish_run(&run, "failed", Some(category), &message, None)?;
                let next = service
                    .current_task()?
                    .ok_or_else(|| Error::Conflict("Current task disappeared".into()))?;
                let budget = next.definition().definition.attempt_limit;
                if matches!(category, "check" | "review" | "validation" | "agent")
                    && budget.is_none_or(|n| next.attempts_used < n)
                    && !stop.load(Ordering::Relaxed)
                {
                    continue;
                }
                return Err(Error::Conflict(format!(
                    "Task #{} stopped ({category}): {message}",
                    run.task
                )));
            }
            Outcome::Passed(_) => unreachable!(),
        }
    }
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
    let context = format!("{answers}\nPrevious attempt: {prior}");
    let role = if repair { "remediation" } else { "execution" };
    let mut evidence = match ai(service, run, ctx, role, &context)? {
        Outcome::Passed(s) => {
            format!(
                "{answers}\nCurrent attempt executor assessment (not independent evidence):\n{s}"
            )
        }
        other => return Ok(other),
    };
    for check in checks {
        let step = service.start_check(run, check)?;
        let cwd = ctx.workspace.root.join(&check.directory).canonicalize()?;
        require(
            cwd.starts_with(&ctx.workspace.root),
            "Check directory escapes project",
        )?;
        service.output(
            run,
            "system",
            &format!("Check {}: {}", check.name, check.command),
        )?;
        let invocation = Invocation {
            executable: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), check.command.clone()],
            cwd,
            prompt: String::new(),
            schema: None,
            scratch_root: ctx.paths.cache.join("workers"),
            worker_lock: ctx.workspace.worker_lock.clone(),
        };
        let mut stream = codex::Stream::default();
        let output_cursor = service.output_cursor(run)?;
        let result = execute(
            service,
            run,
            &step,
            ctx,
            &invocation,
            Some(check.timeout_ms),
            false,
            &mut stream,
        );
        // Keep supervisor evidence outside the worktree and disposable worker scratch.
        let relative = format!("evidence/attempt-{}/step-{}.log", run.attempt, step.id);
        let path = ctx.paths.state.join(&relative);
        std::fs::create_dir_all(path.parent().expect("evidence directory"))?;
        let mut transcript = std::fs::File::create_new(&path)?;
        service.write_output_since(run, output_cursor, &mut transcript)?;
        transcript.sync_all()?;
        service.record_artifact(
            run,
            &check.name,
            &relative,
            transcript.metadata()?.len() as i64,
        )?;
        match result? {
            Outcome::Passed(_) => {
                service.finish_step(&step, true, "Check passed", Some(0))?;
                evidence.push_str(&format!(
                    "\nSupervisor check evidence: {}",
                    serde_json::json!({
                        "name": check.name, "command": check.command, "exit_code": 0,
                        "transcript": path,
                    })
                ));
            }
            Outcome::Failed(category, message) => {
                let message = format!("{message}; supervisor transcript: {}", path.display());
                service.finish_step(&step, false, &message, None)?;
                return Ok(Outcome::Failed(category, message));
            }
            other => return Ok(other),
        }
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
    let step = service.start_ai_step(run, role, model, &scope.to_string_lossy())?;
    service.output(
        run,
        "system",
        &format!(
            "{role}: {}/{} · {}",
            model.provider,
            model.model,
            if step.provider_session.is_some() {
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
        prompt: codex::prompt(run, role, evidence),
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
        return Ok(Outcome::Question(assessment.question.expect("validated")));
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
    let log = std::fs::OpenOptions::new().create(true).append(true).open(
        paths
            .state
            .join(format!("runner-{}.log", service.project_id)),
    )?;
    let mut command = Command::new(&paths.executable);
    command
        .arg("--project")
        .arg(service.project_root()?)
        .args(["run", "--notify"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log)
        .process_group(0);
    if queue {
        command.arg("--queue");
    }
    let mut child = command.spawn()?;
    let output = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(output);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let _ = tx.send(line);
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
        let _ = child.wait();
    });
    let line = rx.recv_timeout(Duration::from_secs(10)).map_err(|_| {
        Error::Conflict(
            "Runner startup still pending; inspect runner log before starting again".into(),
        )
    })?;
    require(
        line.trim() == "ready",
        &format!(
            "Runner could not start; inspect {}",
            paths
                .state
                .join(format!("runner-{}.log", service.project_id))
                .display()
        ),
    )
}
