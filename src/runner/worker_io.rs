//! One worker under the attempt: launched through the guardian, its output streamed into the
//! attempt as it arrives, stopped by Pause, Stop or a time limit.
use super::{
    Context, Outcome, agent, output,
    process::{self, Invocation, Received, Worker},
    provider,
};
use crate::{
    application::{
        Service,
        execution::{Run, Step},
    },
    storage::{Error, Result},
};
use std::time::{Duration, Instant};

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
    let mut worker =
        match Worker::spawn(&ctx.paths.executable, &with_evidence(spec, run, step, ctx)) {
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
        timeout,
        ai,
    };
    let mut sink = Sink::new(stream);
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
            Received::Idle | Received::Artifact => {}
            Received::Output { stream, bytes } => {
                sink.record(service, run, step, &mut worker, (stream, bytes))?;
            }
            Received::Exited(code) => {
                sink.finish(service, run)?;
                ctx.persist_active_time(service, run)?;
                return Ok(interrupted.map_or(Finish::Exited(code), Finish::Stopped));
            }
            Received::Failed(message) => return Err(worker.failure(message)),
            Received::Disconnected => {
                return Err(Error::Conflict(
                    "Worker exited without cleanup confirmation".into(),
                ));
            }
        }
    }
}

/// Why no worker may start now: the task's time limit or the storage reserve.
fn launch_refused(service: &Service, run: &Run, ctx: &Context<'_>) -> Result<Option<Outcome>> {
    ctx.persist_active_time(service, run)?;
    if let Some(outcome) = ctx.deadline_outcome(run) {
        ctx.persist_active_time(service, run)?;
        return Ok(Some(outcome));
    }
    let storage = service.storage_preview()?;
    Ok(storage.work_capacity_reached.then(|| {
        Outcome::DeliveryBlocked(format!(
            "Resource limit: storage capacity reserve reached ({} / {} bytes, {} / {} files/directories); evidence and work retained",
            storage.allocated_bytes,
            storage.effective_max_bytes,
            storage.allocated_files,
            storage.effective_max_files,
        ))
    }))
}

/// The worker's evidence files are archived as this step's.
fn with_evidence(spec: &Invocation, run: &Run, step: &Step, ctx: &Context<'_>) -> Invocation {
    let mut spec = spec.clone();
    spec.artifact_target = Some(process::ArtifactTarget {
        state: ctx.paths.state.clone(),
        project: ctx.workspace.root.clone(),
        scratch_root: ctx.scratch_root(),
        attempt: run.attempt,
        token: run.token.clone(),
        relative_prefix: format!("evidence/attempt-{}/step-{}", run.attempt, step.id),
    });
    spec
}

/// The limits a running worker is held to, besides Pause and Stop.
struct Limit {
    started: Instant,
    /// The stage limit of an AI turn, or a check's own timeout.
    timeout: Option<i64>,
    ai: bool,
}

impl Limit {
    /// Why the worker must stop now, if it must.
    fn reached(&self, service: &Service, run: &Run, ctx: &Context<'_>) -> Result<Option<Outcome>> {
        if ctx.interrupted(service, run)? {
            return Ok(Some(Outcome::Paused));
        }
        if let Some(limit) = self
            .timeout
            .filter(|&n| self.started.elapsed().as_millis() >= n as u128)
        {
            return Ok(Some(Outcome::Failed(
                "timeout",
                if self.ai {
                    format!(
                        "{}: the agent ran {} without returning a result; work and transcript retained",
                        crate::application::stop::STAGE_LIMIT,
                        crate::application::limits::label(Some(limit))
                    )
                } else {
                    "Individual supervisor check timeout reached; work and transcript retained"
                        .into()
                },
            )));
        }
        Ok(ctx.deadline_outcome(run))
    }
}

/// Where a worker's output goes: decoded into stored rows, and through the provider stream
/// for an AI turn.
struct Sink<'s> {
    stream: Option<(&'s mut provider::Stream, &'s str)>,
    /// Codex reports a resumed session's running totals, not the turn's own usage.
    session_totals: bool,
    stdout: output::Utf8Stream,
    stderr: output::Utf8Stream,
}

impl<'s> Sink<'s> {
    fn new(stream: Option<(&'s mut provider::Stream, &'s str)>) -> Self {
        Self {
            session_totals: stream
                .as_ref()
                .is_some_and(|(s, _)| s.provider == provider::Provider::Codex),
            stream,
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
    ) -> Result<()> {
        let mut rows = Vec::new();
        let mut chunk = Some(first);
        let mut chunks = 0;
        while let Some((source, bytes)) = chunk.take() {
            chunks += 1;
            self.decode(service, run, step, &mut rows, source, &bytes)?;
            // Output that queued while the database was busy joins this transaction,
            // bounded so pause and time limits are still checked during a flood.
            if rows.len() < 1000 && chunks < 256 {
                chunk = worker.queued_output();
            }
        }
        service.output_rows(run, &rows)
    }

    fn decode(
        &mut self,
        service: &mut Service,
        run: &Run,
        step: &Step,
        rows: &mut Vec<(String, String)>,
        source: String,
        bytes: &[u8],
    ) -> Result<()> {
        if let Some((stream, role)) = self.stream.as_mut().filter(|_| source == "stdout") {
            for event in stream.feed(bytes)? {
                match event {
                    provider::Event::Text(text) => rows.push(("stdout".into(), text)),
                    provider::Event::Message(reply) => {
                        rows.push(("stdout".into(), agent::display(&reply, role)))
                    }
                    // Keep the stored order: earlier output first.
                    provider::Event::Session(id) => {
                        service.output_rows(run, &std::mem::take(rows))?;
                        service.record_session(step, &id)?
                    }
                    provider::Event::Usage(usage, cost) => {
                        service.output_rows(run, &std::mem::take(rows))?;
                        let usage = if self.session_totals {
                            service.step_share(step, usage)?
                        } else {
                            usage
                        };
                        service.record_usage(step, &usage, cost)?
                    }
                }
            }
            return Ok(());
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
        Ok(())
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
                service.output(run, source, &text)?;
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
