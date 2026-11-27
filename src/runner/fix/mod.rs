//! Agent fix: a detached process that repairs one project on the owner's request and
//! records every change. It shares the project's lease with agent reports, pauses the
//! runner and holds the project folder, preserves work it finds, runs one agent turn in a
//! sandbox that keeps ktask's records and other projects read-only, repeats what failed
//! and says whether it is gone. ktask never starts one by itself.
mod guard;
mod plan;
mod tree;
mod turn;
mod verify;

pub(crate) use guard::main as guarded_git;
pub use plan::{Plan, PlannedStep, plan};

use super::{
    callback::{Capability, FixScope},
    control, investigate,
    process::{self, Invocation, Received, Worker},
    provider::Provider,
    sandbox,
    workspace::{FixHold, Workspace},
};
use crate::{
    application::{
        Actor, ActorKind, ModelConfig, Selector, Service,
        fix::{ChangeKind, Ending, NewFix, ProbeKind, StepSource, StepStatus},
        investigation::{
            FixClass, Investigation, InvestigationLimits, InvestigationState, NewInvestigation,
            ProgressKind,
        },
        preferences::Continuation,
    },
    cli::repair,
    instance::{Instance, Paths},
    storage::{Database, Error, Result},
};
use investigate::{Started, answer, message};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, BufRead, Read},
    path::PathBuf,
    process::ExitCode,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

const INSTRUCTION_CHARS: usize = 4000;
/// How long a fix waits for a runner to stop its worker and leave the project folder.
const PAUSE_PATIENCE: Duration = Duration::from_secs(150);

/// What the owner asked, independent of the transport that carried the request. Exactly
/// one of `report` and `instruction` is set.
#[derive(Clone, Debug)]
pub struct Request {
    pub report: Option<i64>,
    pub instruction: Option<String>,
    /// Continue the queue once the fix is verified.
    pub resume: bool,
    pub requester: Actor,
}

impl Request {
    fn validate(&self) -> Result<()> {
        // Its instruction can permit approvals and answers, so only the owner gives one.
        crate::application::require(
            matches!(self.requester.kind, ActorKind::Human),
            "An agent fix is started by the owner; an AI actor cannot start one",
        )?;
        let instruction = self
            .instruction
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty());
        crate::application::require(
            self.report.is_some() != instruction.is_some(),
            "A fix needs either a report or an instruction",
        )?;
        crate::application::require(
            instruction.is_none_or(|text| text.chars().count() <= INSTRUCTION_CHARS),
            "The instruction must be at most 4000 characters",
        )
    }
}

#[derive(Serialize, Deserialize)]
struct Wire {
    report: Option<i64>,
    instruction: Option<String>,
    resume: bool,
    ai: bool,
    requester: String,
}

/// Starts a detached fix of `project` and returns its ID once it is recorded. Refusals
/// (an agent already running, a report that cannot be carried out, no model, no sandbox)
/// leave no record and change nothing.
pub fn start(paths: &Paths, project: i64, request: &Request) -> Result<i64> {
    request.validate()?;
    let wire = Wire {
        report: request.report,
        instruction: request.instruction.clone(),
        resume: request.resume,
        ai: matches!(request.requester.kind, ActorKind::Ai),
        requester: request.requester.name.clone(),
    };
    investigate::launch(paths, project, "__fix", &wire)
}

/// A fix recorded as running.
struct Fixing {
    service: Service,
    paths: Paths,
    id: i64,
    root: PathBuf,
    model: ModelConfig,
    limits: InvestigationLimits,
    request: Request,
    report: Option<Investigation>,
    token: String,
    _lease: File,
}

impl Fixing {
    fn scratch_root(&self) -> PathBuf {
        turn::scratch_root(&self.paths)
    }

    fn worker_lock(&self) -> PathBuf {
        investigate::worker_lock(&self.paths, self.service.project_id)
    }
}

fn fix_actor(id: i64) -> Actor {
    Actor {
        kind: ActorKind::Ai,
        name: format!("agent fix #{id}"),
    }
}

/// The `__fix PROJECT` process.
pub(crate) fn main(instance: Instance, project: &str) -> ExitCode {
    match fixer(instance, project) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            note!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn fixer(instance: Instance, project: &str) -> Result<()> {
    let stopped = process::stopped()?;
    let mut fixing = match prepare(instance, project) {
        Ok(fixing) => fixing,
        Err(error) => {
            answer(&Started::Refused(message(error)))?;
            return Ok(());
        }
    };
    // A caller that has gone away does not cancel the recorded fix.
    let _ = answer(&Started::Started(fixing.id));
    if let Err(error) = fix(&mut fixing, &stopped) {
        let reason = message(error);
        fixing.service.finish_fix(
            fixing.id,
            &Ending {
                state: InvestigationState::Failed,
                reason: Some(&reason),
                verification: None,
                summary: None,
                continued: false,
            },
        )?;
    }
    Ok(())
}

fn token() -> Result<String> {
    let mut bytes = [0u8; 24];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("fix:{hex}"))
}

/// The model, folder and report a fix needs, checked before anything is recorded.
fn requirements(
    service: &mut Service,
    request: &Request,
) -> Result<(PathBuf, ModelConfig, Option<Investigation>)> {
    request.validate()?;
    let report = request
        .report
        .map(|id| service.fixable_report(id))
        .transpose()?;
    let model = service.operator_model()?.ok_or_else(|| {
        Error::Conflict(
            "No model can fix: set the operator or remediation model (ktask model-default operator --provider codex|claude|opencode --model NAME, or Config > Project models)"
                .into(),
        )
    })?;
    service.validate_model("operator", &model)?;
    let provider = Provider::of(&model)?;
    provider.executable()?;
    // Claude Code and OpenCode cannot hold a writable turn off the network.
    provider.writable_network(service.agent_network()?)?;
    sandbox::available()?;
    let root = service.project_root()?;
    crate::application::require(
        root.is_dir(),
        &format!("The project folder {} is missing", root.display()),
    )?;
    let root = root.canonicalize()?;
    let top = super::workspace::git(&root, &["rev-parse", "--show-toplevel"])?;
    crate::application::require(
        PathBuf::from(top).canonicalize()? == root,
        "The project must be the root of its Git repository",
    )?;
    super::workspace::git(&root, &["rev-parse", "--verify", "HEAD"])
        .map_err(|_| Error::Conflict("The repository needs an initial commit".into()))?;
    Ok((root, model, report))
}

fn prepare(instance: Instance, project: &str) -> Result<Fixing> {
    let mut line = String::new();
    io::stdin().lock().take(64 * 1024).read_line(&mut line)?;
    let wire: Wire = serde_json::from_str(&line)
        .map_err(|error| Error::Conflict(format!("Invalid fix request: {error}")))?;
    let project: i64 = project
        .parse()
        .map_err(|_| Error::Conflict("Invalid project ID".into()))?;
    let paths = instance.paths()?;
    let db = Database::open(&paths)?;
    let mut service = Service::select(db, Selector::Id(project))?
        .ok_or_else(|| Error::Conflict(format!("Project {project} is not registered")))?;
    let request = Request {
        report: wire.report,
        instruction: wire.instruction,
        resume: wire.resume,
        requester: Actor {
            kind: if wire.ai {
                ActorKind::Ai
            } else {
                ActorKind::Human
            },
            name: wire.requester,
        },
    };
    service.set_actor(request.requester.clone())?;
    let lease = investigate::lease(&mut service, &paths)?;
    // Holding the lease: whatever is still recorded as running has no process.
    investigate::abandon(&mut service)?;
    let (root, model, report) = requirements(&mut service, &request)?;
    let limits = service.fix_limits()?;
    let token = token()?;
    let question = request.instruction.clone().or_else(|| {
        request
            .report
            .map(|report| format!("Carry out report #{report}"))
    });
    let id = service.begin_fix(&NewFix {
        run: NewInvestigation {
            task_id: report.as_ref().and_then(|report| report.task_id),
            question: question.as_deref(),
            model: &model,
            limits: &limits,
        },
        report_id: request.report,
        resume: request.resume,
        token: &token,
    })?;
    // Every change from here on is the fix's own, never the owner's.
    service.set_actor(fix_actor(id))?;
    Ok(Fixing {
        service,
        paths,
        id,
        root,
        model,
        limits,
        request,
        report,
        token,
        _lease: lease,
    })
}

/// Why a fix ends before it can verify itself.
type Halt = (InvestigationState, String);

/// The time and cost limits and stop requests every phase of a fix observes.
struct Clock<'a> {
    started: Instant,
    limit: Duration,
    cost_limit: i64,
    cost: Option<i64>,
    stopped: &'a AtomicBool,
    checked: Option<Instant>,
}

impl Clock<'_> {
    fn halted(&mut self, fixing: &mut Fixing) -> Result<Option<Halt>> {
        if self.stopped.load(Ordering::Relaxed) {
            return Ok(Some((
                InvestigationState::Stopped,
                "The fix was interrupted by a signal".into(),
            )));
        }
        if self
            .checked
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(100))
        {
            self.checked = Some(Instant::now());
            if let Some(by) = fixing.service.investigation_stop_requested(fixing.id)? {
                return Ok(Some((
                    InvestigationState::Stopped,
                    format!("Stopped by {by}"),
                )));
            }
        }
        Ok((self.started.elapsed() >= self.limit).then(|| {
            (
                InvestigationState::TimedOut,
                format!(
                    "The time limit of {} passed",
                    crate::presentation::duration(
                        u64::try_from(self.limit.as_millis()).unwrap_or(u64::MAX)
                    )
                ),
            )
        }))
    }

    fn over_budget(&self) -> Option<Halt> {
        self.cost
            .filter(|cost| *cost > self.cost_limit)
            .map(|cost| {
                (
                    InvestigationState::OverBudget,
                    format!(
                        "The cost limit of {} passed: {} used",
                        crate::presentation::report_text::usd(self.cost_limit),
                        crate::presentation::report_text::usd(cost)
                    ),
                )
            })
    }

    /// Runs one command under a guardian: its exit code, whether its own time limit ended
    /// it, and the last of its output.
    fn supervise(
        &mut self,
        fixing: &mut Fixing,
        invocation: &Invocation,
    ) -> verify::Interruptible<(i32, bool, Vec<u8>)> {
        let mut worker = Worker::spawn(&fixing.paths.executable, invocation)?;
        let mut output = Vec::new();
        loop {
            if let Some(halt) = self.halted(fixing)? {
                worker.join()?;
                return Ok(Err(halt));
            }
            match worker.receive(Duration::from_millis(20))? {
                Received::Idle | Received::Note(_) => {}
                Received::Output { bytes, .. } => {
                    output.extend(bytes);
                    let excess = output.len().saturating_sub(4096);
                    output.drain(..excess);
                }
                Received::Exited(code) => return Ok(Ok((code, worker.timed_out(), output))),
                Received::Failed(error) => {
                    let error = message(worker.failure(error));
                    return Ok(Ok((1, false, error.into_bytes())));
                }
                Received::Disconnected => {
                    return Ok(Ok((1, false, b"the guardian disconnected".to_vec())));
                }
            }
        }
    }
}

/// Pauses the project's runner through the task state machine, waits until it has left the
/// project folder, and holds the folder so no runner starts while the fix works.
fn pause(fixing: &mut Fixing) -> Result<FixHold> {
    let hold = Workspace::hold_for_fix(&fixing.root, fixing.id)?;
    let service = &mut fixing.service;
    let deadline = Instant::now() + PAUSE_PATIENCE;
    let (mut was_running, mut paused) = (false, None);
    loop {
        if super::supervisor_alive(service)? {
            was_running = true;
            if let Some(task) = control::pause(service)? {
                paused = task.or(paused);
            }
        } else if service.execution_session()?.is_some_and(|session| {
            matches!(
                session.state.as_str(),
                "running" | "waiting" | "resume_requested"
            )
        }) {
            // No process, but a continuation is pending: it must not start a task.
            match service.request_current_pause() {
                Ok(_) | Err(Error::Conflict(_)) => {}
                Err(error) => return Err(error),
            }
        }
        match Workspace::acquire_for_fix(&fixing.root, &hold) {
            Ok(workspace) => {
                service.settle_orphan()?;
                control::finish_pending_with_workspace(service, &workspace)?;
                break;
            }
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
    service.fix_paused_runner(fixing.id, was_running, paused)?;
    let result = match (was_running, paused) {
        (true, Some(task)) => format!("paused task #{task} and waited for the runner to exit"),
        (true, None) => "stopped the runner and waited for it to exit".into(),
        (false, _) => "no runner was running; none starts while the fix works".into(),
    };
    service.fix_step(
        fixing.id,
        StepSource::Supervisor,
        "Pause the project runner",
        &result,
        StepStatus::Applied,
    )?;
    Ok(hold)
}

/// A report's steps that are never the agent's are recorded as left for the owner.
fn leave_owner_steps(fixing: &mut Fixing) -> Result<()> {
    let Some(report) = fixing
        .report
        .as_ref()
        .and_then(|found| found.report.clone())
    else {
        return Ok(());
    };
    for step in report
        .fix_steps
        .iter()
        .filter(|step| step.class == FixClass::Owner)
    {
        fixing.service.fix_step(
            fixing.id,
            StepSource::Supervisor,
            &step.action,
            "the report marks this step as the owner's",
            StepStatus::LeftForOwner,
        )?;
    }
    if let Some(defect) = &report.ktask_defect {
        fixing.service.fix_step(
            fixing.id,
            StepSource::Supervisor,
            "Repair ktask itself",
            defect,
            StepStatus::LeftForOwner,
        )?;
    }
    Ok(())
}

struct Ended {
    state: InvestigationState,
    reason: Option<String>,
    verification: Option<String>,
    summary: Option<String>,
}

fn fix(fixing: &mut Fixing, stopped: &AtomicBool) -> Result<()> {
    let mut clock = Clock {
        started: Instant::now(),
        limit: Duration::from_millis(fixing.limits.time_limit_ms.unsigned_abs()),
        cost_limit: fixing.limits.cost_limit_microusd,
        cost: None,
        stopped,
        checked: None,
    };
    announce(fixing)?;
    let hold = pause(fixing)?;
    let ended = work(fixing, &mut clock)?;
    finish(fixing, hold, ended)
}

fn announce(fixing: &mut Fixing) -> Result<()> {
    let from = fixing.request.report.map_or_else(
        || "the owner's instruction".to_owned(),
        |id| format!("report #{id}"),
    );
    fixing.service.investigation_progress(
        fixing.id,
        ProgressKind::Status,
        &format!(
            "Fixing from {from} · {} · time limit {} · cost limit {}",
            fixing.model.identifier(),
            crate::presentation::duration(fixing.limits.time_limit_ms.unsigned_abs()),
            crate::presentation::report_text::usd(fixing.limits.cost_limit_microusd)
        ),
    )?;
    if Provider::of(&fixing.model)? == Provider::Codex {
        fixing.service.investigation_progress(
            fixing.id,
            ProgressKind::Status,
            "Codex reports no cost, so the cost limit cannot end this run; the time limit bounds it",
        )?;
    }
    Ok(())
}

/// The repair itself, with the project folder held and its runner paused.
fn work(fixing: &mut Fixing, clock: &mut Clock<'_>) -> Result<Ended> {
    let id = fixing.id;
    leave_owner_steps(fixing)?;
    let scratch = fixing.scratch_root();
    let found = tree::snapshot(&fixing.root, &scratch)?;
    let owner = tree::owner(&mut fixing.service)?;
    let shim = turn::guard(&fixing.paths)?;
    let sandbox = turn::sandbox(fixing, shim.path())?;
    let mut halt = clock.halted(fixing)?;
    if halt.is_none() {
        halt = verify::baseline(fixing, &found, owner, clock)?.err();
    }
    let preserved = if halt.is_none() {
        tree::preserve(&mut fixing.service, id, &fixing.root, &found, owner)?
    } else {
        String::new()
    };
    let start = tree::snapshot(&fixing.root, &scratch)?;
    let (mut unfinished, mut summary) = (None, None);
    if halt.is_none() {
        match turn::run(fixing, &preserved, &sandbox, shim.path(), clock)? {
            Ok(turn::Turn::Replied(reply)) => {
                turn::record_reply(fixing, &reply)?;
                summary = Some(reply.summary);
            }
            Ok(turn::Turn::Failed(why)) => unfinished = Some(why),
            Err(ended) => halt = Some(ended),
        }
    }
    // Recorded however the turn ended: the owner needs to know what changed.
    let mut concerns = tree::record(
        &mut fixing.service,
        id,
        &fixing.root,
        &scratch,
        &found,
        &start,
    )?;
    if halt.is_none() {
        let before = tree::status(&fixing.root)?;
        fixing
            .service
            .investigation_progress(id, ProgressKind::Status, "Verifying the fix")?;
        halt = verify::after(fixing, &sandbox, clock)?.err();
        let owned = tree::owner(&mut fixing.service)?.is_some();
        for line in tree::status(&fixing.root)? {
            if !before.contains(&line) {
                fixing.service.fix_change(
                    id,
                    ChangeKind::Uncommitted,
                    line.trim(),
                    "left by a verification run",
                )?;
                if !owned {
                    concerns.uncommitted.push(line.trim().to_owned());
                }
            }
        }
    }
    // The sandbox of the verification commands names it, so it lasts until here.
    shim.close()?;
    if let Some((state, reason)) = halt {
        return Ok(Ended {
            state,
            verification: Some("Not verified: the fix ended before its verification".into()),
            reason: Some(reason),
            summary,
        });
    }
    let probes = fixing
        .service
        .fix(id)?
        .map_or_else(Vec::new, |fix| fix.probes);
    let verdict = verify::verdict(&probes, &concerns, unfinished.as_deref());
    Ok(Ended {
        state: verdict.state,
        reason: verdict.reason,
        verification: Some(verdict.verification),
        summary,
    })
}

/// Continues the queue after a verified fix when asked, and records the end.
fn finish(fixing: &mut Fixing, hold: FixHold, ended: Ended) -> Result<()> {
    let id = fixing.id;
    let verified = ended.state == InvestigationState::Finished;
    let mut continued = false;
    let (result, status) = if !verified {
        (
            "the runner stays paused: the fix is not verified".to_owned(),
            StepStatus::LeftForOwner,
        )
    } else if !fixing.request.resume {
        (
            "the runner stays paused; ktask run continues the queue".to_owned(),
            StepStatus::LeftForOwner,
        )
    } else {
        drop(hold);
        match resume(fixing) {
            Ok(()) => {
                continued = true;
                ("the runner was started".to_owned(), StepStatus::Applied)
            }
            Err(error) => (
                format!("the queue was not continued: {}", message(error)),
                StepStatus::Failed,
            ),
        }
    };
    fixing.service.fix_step(
        id,
        StepSource::Supervisor,
        "Continue the queue",
        &result,
        status,
    )?;
    fixing.service.finish_fix(
        id,
        &Ending {
            state: ended.state,
            reason: ended.reason.as_deref(),
            verification: ended.verification.as_deref(),
            summary: ended.summary.as_deref(),
            continued,
        },
    )
}

/// The owner's `--resume`: the same detached runner `ktask run` starts.
fn resume(fixing: &mut Fixing) -> Result<()> {
    let queue = match fixing.service.execution_session()? {
        Some(session) => session.mode == "queue",
        None => {
            fixing
                .service
                .preferences(&fixing.paths.config)?
                .continuation
                == Continuation::Queue
        }
    };
    fixing.service.set_actor(fixing.request.requester.clone())?;
    let started = super::background(&mut fixing.service, &fixing.paths, queue, false);
    fixing.service.set_actor(fix_actor(fixing.id))?;
    started
}

fn quoted(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if !arg.is_empty()
                && arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:#,@".contains(c))
            {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn reply(code: i32, text: &str) -> super::android_transport::Output {
    let mut bytes = text.trim_end().as_bytes().to_vec();
    bytes.push(b'\n');
    super::android_transport::Output {
        code,
        stdout: if code == 0 { bytes.clone() } else { Vec::new() },
        stderr: if code == 0 { Vec::new() } else { bytes },
    }
}

/// One command of the running fix, applied by the guardian outside the agent's sandbox.
pub(super) fn command(
    capability: &Capability,
    scope: &FixScope,
    token: &str,
    args: &[String],
    cancelled: &AtomicBool,
) -> Result<super::android_transport::Output> {
    let db = Database::open_in(&capability.state)?;
    let mut service = Service::select(db, Selector::Id(scope.project_id))?
        .ok_or_else(|| Error::Conflict("The fixed project is not registered".into()))?;
    let id = scope.id;
    service.require_running_fix(id, token)?;
    service.set_actor(fix_actor(id))?;
    let paths = Paths {
        config: scope.config.clone(),
        state: capability.state.clone(),
        cache: scope.cache.clone(),
        data: scope.data.clone(),
        runtime: scope.runtime.clone(),
        executable: scope.executable.clone(),
    };
    let word = args.first().map(String::as_str).unwrap_or_default();
    let rest = args.get(1..).unwrap_or_default();
    match word {
        "step" | "left" => {
            let Some(action) = rest.first().filter(|text| !text.trim().is_empty()) else {
                return Ok(reply(2, "Say what was done, or what is left"));
            };
            let status = if word == "step" {
                StepStatus::Applied
            } else {
                StepStatus::LeftForOwner
            };
            service.fix_step(id, StepSource::Agent, action, &rest[1..].join(" "), status)?;
            Ok(reply(0, "recorded"))
        }
        "refused-git" => {
            let line = rest.first().map_or("git", String::as_str);
            let why = rest.get(1).map_or("not allowed in a fix", String::as_str);
            service.fix_step(id, StepSource::Git, line, why, StepStatus::Refused)?;
            Ok(reply(0, "recorded"))
        }
        "probe" => {
            let command = rest.join(" ");
            if command.trim().is_empty() {
                return Ok(reply(2, "Name the command to repeat"));
            }
            if turn::repeats(&service, id, &command)? {
                return Ok(reply(0, "already repeated"));
            }
            let invocation = verify::shell(
                &capability.project,
                turn::scratch_root(&paths),
                investigate::probe_lock(&paths, scope.project_id),
                &scope.sandbox,
                &command,
            );
            let (passed, detail) = verify::command(
                &paths.executable,
                &invocation,
                Duration::from_millis(scope.command_limit_ms),
                cancelled,
            );
            service.fix_probe(
                id,
                ProbeKind::Command,
                &command,
                Some(passed),
                &format!("before: {detail}"),
            )?;
            Ok(reply(
                0,
                &format!(
                    "probe recorded; it {} now ({detail})",
                    if passed { "passes" } else { "fails" }
                ),
            ))
        }
        _ => ktask(&mut service, &paths, &capability.project, id, args),
    }
}

/// A ktask command: applied or refused, and recorded either way.
fn ktask(
    service: &mut Service,
    paths: &Paths,
    root: &std::path::Path,
    id: i64,
    args: &[String],
) -> Result<super::android_transport::Output> {
    let gone = || Error::Conflict("The fix record disappeared".into());
    let found = service.investigation(id)?.ok_or_else(gone)?;
    let record = service.fix(id)?.ok_or_else(gone)?;
    let line = format!("ktask {}", quoted(args));
    let authority = repair::Authority {
        fix: id,
        // A report's text was written by an agent; only the owner's instruction permits.
        instruction: found
            .question
            .as_deref()
            .filter(|_| record.report_id.is_none()),
        root,
    };
    Ok(match repair::run(service, paths, &authority, args) {
        repair::Answer::Read(text) => reply(0, &text),
        repair::Answer::Applied { result, changes } => {
            service.fix_step(id, StepSource::Ktask, &line, &result, StepStatus::Applied)?;
            for (name, change) in changes {
                service.fix_change(id, ChangeKind::Setting, &name, &change)?;
            }
            service.fix_event(id, &line)?;
            reply(0, &result)
        }
        repair::Answer::Refused(why) => {
            service.fix_step(id, StepSource::Ktask, &line, &why, StepStatus::Refused)?;
            reply(4, &format!("refused and left for the owner: {why}"))
        }
        repair::Answer::Failed(error) => {
            service.fix_step(id, StepSource::Ktask, &line, &error, StepStatus::Failed)?;
            reply(1, &error)
        }
    })
}
