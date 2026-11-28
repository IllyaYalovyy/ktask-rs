//! One worker under the attempt: launched through the guardian, its output streamed into the
//! attempt as it arrives, stopped by Pause, Stop or a time limit.
use super::{
    Context, Outcome, agent,
    evidence::ArtifactTarget,
    output,
    process::{Invocation, Received, Worker},
    provider,
};
use crate::{
    application::{
        Service,
        execution::{Run, Step},
    },
    storage::{Error, Result},
};
use std::{
    ops::ControlFlow,
    time::{Duration, Instant},
};

/// How a worker process ended.
pub(super) enum Finish {
    /// It exited by itself with this code.
    Exited(i32),
    /// The supervisor ended it first, or could not start it: pause, a time limit, a launch
    /// failure.
    Stopped(Outcome),
}

/// Runs one worker to its end, streaming its output into the attempt. An AI turn passes its
/// provider stream; a check passes none and its output is stored as written. Callers record
/// the step's result.
pub(super) fn execute(
    service: &mut Service,
    run: &Run,
    step: &Step,
    ctx: &mut Context<'_>,
    spec: &Invocation,
    timeout: Option<i64>,
    stream: Option<(&mut provider::Stream, &str)>,
) -> Result<Finish> {
    if let Some(outcome) = launch_refused(service, run, ctx)? {
        return Ok(Finish::Stopped(outcome));
    }
    let ai = stream.is_some();
    // A check's completed collection says what it left, also when that is nothing.
    let mut spec = with_evidence(spec, run, step, ctx, service.project_id, !ai);
    spec.scratch_limits = Some(service.scratch_limits(run.attempt)?);
    // A check's timeout is for its command, which only the guardian can time exactly. An
    // agent's stage limit stays here.
    if !ai {
        spec.time_limit_ms = timeout.map(i64::unsigned_abs);
    }
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
    let limit = Limit {
        started: Instant::now(),
        stage_ms: timeout.filter(|_| ai),
    };
    let mut sink = Sink::new(stream, |service| limit.reached(service, run, ctx));
    let mut interrupted = None;
    let mut last_tick = Instant::now();
    loop {
        if interrupted.is_none() {
            interrupted = limit.reached(service, run, ctx)?;
            if interrupted.is_some() {
                worker.stop();
            }
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            ctx.persist_active_time(service, run)?;
            last_tick = Instant::now();
        }
        match worker.receive(Duration::from_millis(40))? {
            Received::Idle => {}
            Received::Note(note) => service.note(run, &note)?,
            Received::Output { stream, bytes } if interrupted.is_none() => {
                if let ControlFlow::Break(outcome) =
                    sink.record(service, run, step, &mut worker, (stream, bytes))?
                {
                    worker.stop();
                    interrupted = Some(outcome);
                }
            }
            Received::Output { .. } => {}
            Received::Exited(code) => {
                let (bytes, entries) = worker.scratch_peak();
                service.record_scratch_peak(run.attempt, bytes, entries)?;
                if interrupted.is_none() {
                    sink.finish(service, run)?;
                }
                ctx.persist_active_time(service, run)?;
                let ended = if worker.timed_out() {
                    Finish::Stopped(Outcome::CheckTimedOut)
                } else {
                    Finish::Exited(code)
                };
                return Ok(interrupted.map_or(ended, Finish::Stopped));
            }
            Received::Failed(message) => {
                let error = worker.failure(message);
                return interrupted.map_or(Err(error), |outcome| Ok(Finish::Stopped(outcome)));
            }
            Received::Disconnected => {
                return interrupted.map_or(
                    Err(Error::Conflict(
                        "Worker exited without cleanup confirmation".into(),
                    )),
                    |outcome| Ok(Finish::Stopped(outcome)),
                );
            }
        }
    }
}

/// Why no worker may start now: the task's time limit or the storage reserve.
fn launch_refused(service: &mut Service, run: &Run, ctx: &Context<'_>) -> Result<Option<Outcome>> {
    ctx.persist_active_time(service, run)?;
    if let Some(outcome) = ctx.deadline_outcome(run) {
        ctx.persist_active_time(service, run)?;
        return Ok(Some(outcome));
    }
    match service.retry_storage_until(
        |service| match service.work_capacity()?.refusal() {
            Some(refusal) => Err(Error::Capacity(Box::new(refusal))),
            None => Ok(()),
        },
        |service| admission_interrupted(service, run, ctx),
    ) {
        Ok(ControlFlow::Continue(())) => Ok(None),
        Ok(ControlFlow::Break(outcome)) => Ok(Some(outcome)),
        Err(Error::Capacity(refusal)) => Ok(Some(Outcome::StorageBlocked(refusal))),
        Err(error) => Err(error),
    }
}

pub(super) fn producer_notice(
    service: &mut Service,
    run: &Run,
    ctx: &Context<'_>,
    text: &str,
) -> Result<ControlFlow<Outcome>> {
    service.retry_storage_until(
        |service| service.output(run, "system", text),
        |service| admission_interrupted(service, run, ctx),
    )
}

fn admission_interrupted(
    service: &Service,
    run: &Run,
    ctx: &Context<'_>,
) -> Result<Option<Outcome>> {
    if ctx.interrupted(service, run)? {
        Ok(Some(Outcome::Paused))
    } else {
        Ok(ctx.deadline_outcome(run))
    }
}

/// The worker's evidence files are archived as this step's.
fn with_evidence(
    spec: &Invocation,
    run: &Run,
    step: &Step,
    ctx: &Context<'_>,
    project_id: i64,
    record_empty: bool,
) -> Invocation {
    let mut spec = spec.clone();
    spec.artifact_target = Some(ArtifactTarget {
        state: ctx.paths.state.clone(),
        project_id,
        project: ctx.workspace.root.clone(),
        scratch_root: ctx.scratch_root(),
        attempt: run.attempt,
        token: run.token.clone(),
        relative_prefix: format!("evidence/attempt-{}/step-{}", run.attempt, step.id),
        record_empty,
    });
    spec
}

/// The limits a running worker is held to, besides Pause and Stop.
struct Limit {
    started: Instant,
    /// The stage limit of an AI turn.
    stage_ms: Option<i64>,
}

impl Limit {
    /// Why the worker must stop now, if it must.
    fn reached(&self, service: &Service, run: &Run, ctx: &Context<'_>) -> Result<Option<Outcome>> {
        if let Some(reason) = service.pending_dependency(run.attempt)? {
            return Ok(Some(Outcome::Dependency(reason)));
        }
        if ctx.interrupted(service, run)? {
            return Ok(Some(Outcome::Paused));
        }
        if let Some(outcome) = ctx.deadline_outcome(run) {
            return Ok(Some(outcome));
        }
        if let Some(limit) = self
            .stage_ms
            .filter(|&n| self.started.elapsed().as_millis() >= n as u128)
        {
            return Ok(Some(Outcome::Failed(
                "timeout",
                format!(
                    "{}: the agent ran {} without returning a result; work and transcript retained",
                    crate::application::stop::STAGE_LIMIT,
                    crate::application::limits::label(Some(limit))
                ),
            )));
        }
        Ok(None)
    }
}

/// Where a worker's output goes: decoded into stored rows, and through the provider stream
/// for an AI turn.
struct Sink<'s, F> {
    stream: Option<(&'s mut provider::Stream, &'s str)>,
    interrupted: F,
    /// Codex reports a resumed session's running totals, not the turn's own usage.
    session_totals: bool,
    stdout: output::Utf8Stream,
    stderr: output::Utf8Stream,
}

impl<'s, F: FnMut(&Service) -> Result<Option<Outcome>>> Sink<'s, F> {
    fn new(stream: Option<(&'s mut provider::Stream, &'s str)>, interrupted: F) -> Self {
        Self {
            session_totals: stream
                .as_ref()
                .is_some_and(|(s, _)| s.provider == provider::Provider::Codex),
            stream,
            interrupted,
            stdout: output::Utf8Stream::default(),
            stderr: output::Utf8Stream::default(),
        }
    }

    /// Stores `first` and the output queued behind it in one transaction.
    fn record(
        &mut self,
        service: &mut Service,
        run: &Run,
        step: &Step,
        worker: &mut Worker,
        first: (String, Vec<u8>),
    ) -> Result<ControlFlow<Outcome>> {
        let mut rows = Vec::new();
        let mut chunk = Some(first);
        let mut chunks = 0;
        while let Some((source, bytes)) = chunk.take() {
            chunks += 1;
            if let ControlFlow::Break(outcome) =
                self.decode(service, run, step, &mut rows, source, &bytes)?
            {
                return Ok(ControlFlow::Break(outcome));
            }
            // Output that queued while the database was busy joins this transaction,
            // bounded so pause and time limits are still checked during a flood.
            if rows.len() < 1000 && chunks < 256 {
                chunk = worker.queued_output();
            }
        }
        store_rows(service, run, &rows, &mut self.interrupted)
    }

    fn decode(
        &mut self,
        service: &mut Service,
        run: &Run,
        step: &Step,
        rows: &mut Vec<(String, String)>,
        source: String,
        bytes: &[u8],
    ) -> Result<ControlFlow<Outcome>> {
        if let Some((stream, role)) = self.stream.as_mut().filter(|_| source == "stdout") {
            for event in stream.feed(bytes)? {
                match event {
                    provider::Event::Text(text) => rows.push(("stdout".into(), text)),
                    provider::Event::Message(reply) => {
                        rows.push(("stdout".into(), agent::display(&reply, role)))
                    }
                    // Keep the stored order: earlier output first.
                    provider::Event::Session(id) => {
                        if let ControlFlow::Break(outcome) =
                            store_rows(service, run, &std::mem::take(rows), &mut self.interrupted)?
                        {
                            return Ok(ControlFlow::Break(outcome));
                        }
                        service.record_session(step, &id)?
                    }
                    provider::Event::Usage(usage, cost) => {
                        if let ControlFlow::Break(outcome) =
                            store_rows(service, run, &std::mem::take(rows), &mut self.interrupted)?
                        {
                            return Ok(ControlFlow::Break(outcome));
                        }
                        let usage = if self.session_totals {
                            service.step_share(step, usage)?
                        } else {
                            usage
                        };
                        service.record_usage(step, &usage, cost)?
                    }
                }
            }
            return Ok(ControlFlow::Continue(()));
        }
        let decoder = if source == "stdout" {
            &mut self.stdout
        } else {
            &mut self.stderr
        };
        let text = decoder.feed(bytes);
        if !text.is_empty() {
            self.note(&source, &text);
            rows.push((source, text));
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Stores what the decoders still hold once the worker has exited.
    fn finish(&mut self, service: &mut Service, run: &Run) -> Result<()> {
        for source in ["stdout", "stderr"] {
            let text = if source == "stdout" {
                self.stdout.finish()
            } else {
                self.stderr.finish()
            };
            if !text.is_empty() {
                self.note(source, &text);
                if let Err(error) = service.output(run, source, &text) {
                    match error {
                        Error::Capacity(_) => {
                            note!("Final {source} fragment could not be retained ({error})")
                        }
                        other => return Err(other),
                    }
                }
            }
        }
        Ok(())
    }

    /// A provider's stderr explains its failure.
    fn note(&mut self, source: &str, text: &str) {
        if source == "stderr"
            && let Some((stream, _)) = self.stream.as_mut()
        {
            stream.note_error(text);
        }
    }
}

fn store_rows(
    service: &mut Service,
    run: &Run,
    rows: &[(String, String)],
    interrupted: impl FnMut(&Service) -> Result<Option<Outcome>>,
) -> Result<ControlFlow<Outcome>> {
    service.retry_storage_until(|service| service.output_rows(run, rows), interrupted)
}
