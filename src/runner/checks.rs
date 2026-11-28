use super::{
    Context, Outcome,
    backoff::{self, Notice},
    failure_summary, test_summary,
    worker_io::{Finish, execute, producer_notice},
};
use crate::{
    application::{
        CheckCounts, Service,
        artifacts::Publication,
        execution::{Check, CheckResult, Run},
        require,
    },
    storage::{Error, Result},
};
use std::{
    io::{self, Write},
    ops::ControlFlow,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

/// Tells a check the commit the change under verification starts from.
const BASE_COMMIT: &str = "KTASK_BASE_COMMIT";

pub(super) fn run(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    checks: &[Check],
    // The source being verified and the commit its change starts from.
    (tree, base): (&str, &str),
) -> Result<Outcome> {
    let mut evidence = String::new();
    for (index, check) in checks.iter().enumerate() {
        if let Some(wait) =
            ctx.environment_wait(service, run, &format!("before check {}", check.name))?
        {
            return Ok(wait);
        }
        let observed = ctx.workspace.fingerprint(&ctx.scratch_root())?;
        if observed != tree {
            let paths = ctx.workspace.tree_difference(tree, &observed)?;
            let when = if index == 0 {
                "before the first check launched"
            } else {
                "after an earlier check, before the next check launched"
            };
            return Ok(Outcome::Changes {
                report: format!(
                    "Worktree changed {when}; check {} cannot verify a stable source state. Expected tree {tree}; observed tree {observed}; differing paths: {paths}. Make verification read-only with respect to project source.",
                    check.name
                ),
                critical: true,
            });
        }
        let mut check = check.clone();
        if let Some(extended) = service.check_extension_for(run, check.id)? {
            check.timeout_ms = extended;
        }
        match recorded_result(service, run, ctx, &check, tree)? {
            Some(Outcome::Passed(recorded)) => {
                evidence.push_str(&recorded);
                continue;
            }
            Some(failed) => return Ok(failed),
            None => {}
        }
        if let Some(source) = service.reusable_check(&check, tree, &ctx.tools.fingerprint)? {
            let transcript = ctx.paths.state.join(&source.transcript);
            if !source.transcript.is_empty() && transcript.is_file() {
                service.reuse_check(
                    run,
                    &check,
                    tree,
                    &ctx.tools.fingerprint,
                    &source,
                    &transcript,
                )?;
                service.note(
                run,
                &format!(
                    "Check {}: reused passing result from attempt #{} step #{} · transcript: {}",
                    check.name, source.attempt, source.step, transcript.display()
                ),
            )?;
                evidence.push_str(&format!(
                    "\nSupervisor check evidence: {}",
                    serde_json::json!({
                        "name": check.name, "command": check.command, "exit_code": 0, "timeout_ms": check.timeout_ms,
                        "environment_fingerprint": ctx.tools.fingerprint,
                        "transcript": transcript, "reused_from_attempt": source.attempt,
                        "reused_from_step": source.step, "current_counts": source.counts,
                        "previous_checkpoint_counts": service.previous_check_counts(run, check.id)?,
                    })
                ));
                continue;
            }
        }
        if service.reconsider_check_timeout(run, &check, tree, &ctx.tools.fingerprint)? {
            return Ok(Outcome::CheckTimedOut);
        }
        match backoff::bounded(
            service,
            run,
            ctx,
            Notice {
                failed: "Check infrastructure failed",
                retained: "transcript retained",
            },
            |outcome| matches!(outcome, Outcome::Failed("environment", _)),
            |service, ctx| run_recorded(service, run, ctx, &check, (tree, base)),
        )? {
            Outcome::Passed(recorded) => evidence.push_str(&recorded),
            other => return Ok(other),
        }
    }
    evidence.push_str(&service.artifact_manifest(run.attempt, &ctx.paths.state)?);
    Ok(Outcome::Passed(evidence))
}

/// A check this attempt already ran on this source, whose transcript could not be archived
/// then: archive it from the retained output and use the recorded result without running the
/// check again.
fn recorded_result(
    service: &mut Service,
    run: &Run,
    ctx: &Context<'_>,
    check: &Check,
    tree: &str,
) -> Result<Option<Outcome>> {
    let Some(pending) = service.pending_transcript(run, check, tree, &ctx.tools.fingerprint)?
    else {
        return Ok(None);
    };
    let transcript = Transcript {
        relative: &pending.relative,
        name: &check.name,
        tree,
        output: pending.output,
        bytes: pending.bytes,
    };
    let log = archive_recorded(service, run, ctx, pending.step, transcript)?;
    if matches!(log, CheckLog::Archived(_)) {
        service.note(
            run,
            &format!(
                "Check {}: transcript archived from the retained output; its recorded result stands and it is not run again",
                check.name
            ),
        )?;
    }
    let outcome = if pending.passed {
        Outcome::Passed(pending.report)
    } else {
        Outcome::Failed("check", pending.report)
    };
    report_with_log(service, run, ctx, check, outcome, &log, pending.counts).map(Some)
}

enum CheckLog {
    Archived(PathBuf),
    Pending { step: i64, output: (i64, i64) },
    Unarchived,
}

impl CheckLog {
    fn description(&self) -> String {
        match self {
            Self::Archived(path) => format!("Full log: {}", path.display()),
            Self::Pending {
                step,
                output: (start, end),
            } => format!(
                "Transcript publication pending for check step #{step}. Read retained output rows {start} (exclusive) through {end} in the evidence packet."
            ),
            Self::Unarchived => "Captured output remains in the attempt log.".into(),
        }
    }
}

/// Runs `check` once. A pass carries its evidence entry; a failure identifies the retained log.
fn run_recorded(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    check: &Check,
    source: (&str, &str),
) -> Result<Outcome> {
    let (outcome, log, counts) = run_one(service, run, ctx, check, source)?;
    report_with_log(service, run, ctx, check, outcome, &log, counts)
}

fn report_with_log(
    service: &Service,
    run: &Run,
    ctx: &Context<'_>,
    check: &Check,
    outcome: Outcome,
    log: &CheckLog,
    counts: Option<CheckCounts>,
) -> Result<Outcome> {
    Ok(match outcome {
        Outcome::Passed(_) => {
            let path = match log {
                CheckLog::Archived(path) => Some(path),
                CheckLog::Pending { .. } | CheckLog::Unarchived => None,
            };
            let mut evidence = serde_json::json!({
                "name": check.name, "command": check.command, "exit_code": 0, "timeout_ms": check.timeout_ms,
                "environment_fingerprint": ctx.tools.fingerprint,
                "transcript": path, "current_counts": counts,
                "previous_checkpoint_counts": service.previous_check_counts(run, check.id)?,
            });
            if let CheckLog::Pending { .. } = log {
                evidence["transcript_pending"] = log.description().into();
            }
            Outcome::Passed(format!("\nSupervisor check evidence: {evidence}"))
        }
        Outcome::Failed(category, message) => Outcome::Failed(
            category,
            format!(
                "{message}\n{}{}",
                log.description(),
                service.artifact_manifest(run.attempt, &ctx.paths.state)?
            ),
        ),
        other => other,
    })
}

fn archive_recorded(
    service: &mut Service,
    run: &Run,
    ctx: &Context<'_>,
    step: i64,
    transcript: Transcript<'_>,
) -> Result<CheckLog> {
    let output = transcript.output;
    // An older pending row will be sized by the bounded recovery sweep.
    if transcript.bytes.is_none() {
        return Ok(CheckLog::Pending { step, output });
    }
    handle_recorded_archive(service, run, step, output, |service| {
        archive_transcript(service, run, ctx, transcript)
    })
}

fn handle_recorded_archive(
    service: &mut Service,
    run: &Run,
    step: i64,
    output: (i64, i64),
    archive: impl FnOnce(&mut Service) -> Result<PathBuf>,
) -> Result<CheckLog> {
    match archive(service) {
        Ok(path) => {
            service.settle_transcript(step)?;
            Ok(CheckLog::Archived(path))
        }
        Err(error) => {
            let reason = match &error {
                Error::Breach(breach) => format!("{}: {}", breach.limit, breach.reason),
                Error::Capacity(refusal) => refusal.reason.clone(),
                Error::Io(error) if error.kind() == io::ErrorKind::StorageFull => error.to_string(),
                _ => return Err(error),
            };
            service.transcript_deferred(step, &reason)?;
            let log = CheckLog::Pending { step, output };
            service.note(run, &format!("{} {reason}", log.description()))?;
            Ok(log)
        }
    }
}

fn run_one(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    check: &Check,
    (tree, base): (&str, &str),
) -> Result<(Outcome, CheckLog, Option<CheckCounts>)> {
    let step = service.start_check(run, check, &ctx.tools.fingerprint)?;
    let cwd = ctx.workspace.root.join(&check.directory).canonicalize()?;
    require(
        cwd.starts_with(&ctx.workspace.root),
        "Check directory escapes project",
    )?;
    if let ControlFlow::Break(outcome) = producer_notice(
        service,
        run,
        ctx,
        &format!(
            "Check {}: {} (timeout {} ms)",
            check.name, check.command, check.timeout_ms
        ),
    )? {
        return Ok((outcome, CheckLog::Unarchived, None));
    }
    let mut invocation = ctx.invocation(
        PathBuf::from("/bin/sh"),
        vec!["-c".into(), check.command.clone()],
        cwd,
    );
    // A check that compares against what the change started from, such as approved screens,
    // needs it also once the work is committed.
    invocation.env.push((BASE_COMMIT.into(), base.to_owned()));
    if !ctx.tools.installations.is_empty() {
        invocation.sandbox = Some(super::sandbox::Sandbox::for_installations(
            &ctx.tools.installations,
        )?);
    }
    let cursor = service.output_cursor(run)?;
    let finish = execute(
        service,
        run,
        &step,
        ctx,
        &invocation,
        Some(check.timeout_ms),
        None,
    );
    let relative = format!("evidence/attempt-{}/step-{}.log", run.attempt, step.id);
    if matches!(finish, Ok(Finish::Stopped(Outcome::CheckTimedOut))) {
        let end = service.output_cursor(run)?;
        let report = format!(
            "Check {} timed out after {} ms.\nCommand: {}\nTranscript publication pending. See output captured for check step #{}.",
            check.name, check.timeout_ms, check.command, step.id,
        );
        service.record_check_timeout(run, &step, tree, &report, (cursor, end))?;
        return Ok((Outcome::CheckTimedOut, CheckLog::Unarchived, None));
    }
    let transcript_tree = ctx.workspace.fingerprint(&ctx.scratch_root())?;
    let output = (cursor, service.output_cursor(run)?);
    let transcript_of = |bytes| Transcript {
        relative: &relative,
        name: &check.name,
        tree: &transcript_tree,
        output,
        bytes,
    };
    let (outcome, exit, counts, bytes) = match finish {
        Ok(Finish::Exited(code)) => {
            let mut bytes = Vec::new();
            service.write_output_range(run.attempt, output, &mut bytes)?;
            let (outcome, counts) =
                judge_output(&check.name, code, &String::from_utf8_lossy(&bytes));
            (outcome, Some(code), counts, Some(bytes.len() as i64))
        }
        // A check that could not be launched is a recorded failure without an exit status.
        Ok(Finish::Stopped(failed @ Outcome::Failed(..))) => (failed, None, None, None),
        other => {
            let log = match publish_transcript(
                service,
                ctx.paths,
                (run.attempt, &run.token),
                transcript_of(None),
                false,
                None,
            ) {
                Ok(path) => CheckLog::Archived(path),
                Err(error) => {
                    note!(
                        "Check {} stopped; transcript could not be archived ({error})",
                        check.name
                    );
                    CheckLog::Unarchived
                }
            };
            return match other? {
                Finish::Stopped(outcome) => Ok((outcome, log, None)),
                Finish::Exited(_) => unreachable!("handled above"),
            };
        }
    };
    let environment_changed = service
        .resolve_tools(ctx.paths)
        .map_or(true, |current| current.fingerprint != ctx.tools.fingerprint);
    let environment_report = environment_changed.then(|| {
        format!(
            "Tool environment changed during check {}; its result cannot be reused",
            check.name
        )
    });
    let (passed, report) = if let Some(report) = &environment_report {
        (false, report)
    } else {
        match &outcome {
            Outcome::Passed(report) => (true, report),
            Outcome::Failed(_, report) => (false, report),
            _ => unreachable!("a finished check passes or fails"),
        }
    };
    service.finish_check_with_output(
        &step,
        CheckResult {
            passed,
            report,
            exit,
            counts,
        },
        (&relative, &transcript_tree),
        (output, bytes),
        !environment_changed && transcript_tree == tree,
    )?;
    let log = if exit.is_some() {
        archive_recorded(service, run, ctx, step.id, transcript_of(bytes))?
    } else {
        match archive_transcript(service, run, ctx, transcript_of(bytes)) {
            Ok(path) => CheckLog::Archived(path),
            Err(Error::Capacity(_)) => CheckLog::Unarchived,
            Err(error) => return Err(error),
        }
    };
    if let Some(report) = environment_report {
        service.request_tool_environment_wait(run, &report)?;
        return Ok((Outcome::Dependency(report), log, counts));
    }
    if let Some(counts) = counts {
        service.note(
            run,
            &format!(
                "Check {}: {} passed, {} failed, {} ignored",
                check.name, counts.passed, counts.failed, counts.ignored
            ),
        )?;
    }
    Ok((outcome, log, counts))
}

/// The verdict on an exited check from its exit status and its own test summaries.
fn judge_output(name: &str, code: i32, transcript: &str) -> (Outcome, Option<CheckCounts>) {
    let counts = test_summary::parse(transcript);
    let suites = super::check_report::parse(transcript);
    let suite_json = serde_json::to_string(&suites).expect("suite serialization");
    let verdict = match verdict(name, code, counts) {
        Outcome::Passed(_) if suites.blocks() => Outcome::Failed(
            "check",
            format!("Check {name} has failed, crashed, incomplete or unknown suites"),
        ),
        other => other,
    };
    let outcome = match verdict {
        Outcome::Failed(category, headline) => Outcome::Failed(
            category,
            format!(
                "{headline}\n{}\nSuite results: {suite_json}",
                failure_summary::summarize(transcript).join("\n")
            ),
        ),
        Outcome::Passed(message) => {
            Outcome::Passed(format!("{message}\nSuite results: {suite_json}"))
        }
        other => other,
    };
    (outcome, counts)
}

pub(super) struct Transcript<'a> {
    pub relative: &'a str,
    pub name: &'a str,
    pub tree: &'a str,
    pub output: (i64, i64),
    pub bytes: Option<i64>,
}

pub(super) fn archive_transcript(
    service: &mut Service,
    run: &Run,
    ctx: &Context<'_>,
    transcript: Transcript<'_>,
) -> Result<PathBuf> {
    publish_transcript(
        service,
        ctx.paths,
        (run.attempt, &run.token),
        transcript,
        true,
        None,
    )
}

/// Recovery never runs on the launch path. Small logs share a fixed page budget.
/// A larger eligible log gets its own pass, bounded by its enforced per-file limit.
const RECOVERY_BYTES: u64 = 16 * 1024 * 1024;

pub(super) fn archive_pending(
    service: &mut Service,
    paths: &crate::instance::Paths,
    stop: &AtomicBool,
) {
    if let Err(error) = recover_logs(service, paths, stop, RECOVERY_BYTES, || {
        stop.load(Ordering::Relaxed)
    }) {
        note!("Cannot recover pending check transcripts ({error})");
    }
}

fn recover_logs(
    service: &mut Service,
    paths: &crate::instance::Paths,
    stop: &AtomicBool,
    budget: u64,
    mut interrupted: impl FnMut() -> bool,
) -> Result<()> {
    let mut cursor = service.transcript_recovery_cursor()?;
    let mut logs = service.deferred_check_logs(cursor)?;
    if logs.is_empty() && cursor != 0 {
        logs = service.deferred_check_logs(0)?;
    }
    let mut spent = 0;
    for log in logs {
        if spent >= budget || interrupted() {
            break;
        }
        let mut bytes = log.bytes;
        if bytes.is_none() {
            let Some(page) =
                service.size_legacy_transcript(&log, budget - spent, &mut interrupted)?
            else {
                break;
            };
            spent += page.spent;
            bytes = page.bytes;
        }
        if interrupted() {
            break;
        }
        if let Some(size) = bytes {
            let size = size as u64;
            if size <= service.transcript_file_limit(log.attempt)? {
                if spent > 0 && size > budget - spent {
                    break;
                }
                let result = publish_transcript(
                    service,
                    paths,
                    (log.attempt, &log.token),
                    Transcript {
                        relative: &log.relative,
                        name: &log.name,
                        tree: &log.tree,
                        output: log.output,
                        bytes,
                    },
                    false,
                    Some(stop),
                );
                if interrupted() {
                    break;
                }
                match result {
                    Ok(_) => service.settle_transcript(log.step)?,
                    Err(error) => note!(
                        "Check step #{} transcript still pending ({error})",
                        log.step
                    ),
                }
                spent = spent.saturating_add(size);
            }
        }
        if !service.advance_transcript_recovery(cursor, log.step)? {
            break;
        }
        cursor = log.step;
    }
    Ok(())
}

struct TranscriptWriter<'a> {
    file: &'a mut std::fs::File,
    stop: &'a AtomicBool,
}

impl Write for TranscriptWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.stop.load(Ordering::Relaxed) {
            return Err(io::Error::other("Transcript recovery interrupted"));
        }
        self.file.write(&bytes[..bytes.len().min(64 * 1024)])
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn publish_transcript(
    service: &mut Service,
    paths: &crate::instance::Paths,
    owner: (i64, &str),
    transcript: Transcript<'_>,
    reclaim: bool,
    stop: Option<&AtomicBool>,
) -> Result<PathBuf> {
    publish_transcript_with(
        service,
        paths,
        owner,
        transcript,
        reclaim,
        stop,
        |service, publication| {
            if let Some(stop) = stop {
                return service.publish_artifact_until(publication, stop);
            }
            service.publish_artifact(
                publication.attempt,
                publication.token,
                publication.name,
                publication.source,
                publication.relative,
                publication.tree,
            )
        },
    )
}

fn publish_transcript_with(
    service: &mut Service,
    paths: &crate::instance::Paths,
    (attempt, token): (i64, &str),
    transcript: Transcript<'_>,
    reclaim: bool,
    stop: Option<&AtomicBool>,
    publish_artifact: impl Fn(&Service, Publication<'_>) -> Result<()>,
) -> Result<PathBuf> {
    if let Some(path) =
        service.retained_check_transcript(attempt, &paths.state, transcript.relative)?
    {
        return Ok(path);
    }
    // Preflight with the judged byte count before writing to scratch.
    let mut extensions = 0;
    if let Some(bytes) = transcript.bytes {
        while let Err(error) =
            service.require_artifact_file_budget(attempt, transcript.name, bytes as u64)
        {
            if !extend_archive_limit(service, attempt, reclaim, &mut extensions, &error)? {
                return Err(transcript_publication_error(error));
            }
        }
    }
    let scratch = stage_transcript(paths, |output| {
        if let Some(stop) = stop {
            service.write_output_range(
                attempt,
                transcript.output,
                &mut TranscriptWriter { file: output, stop },
            )
        } else {
            service.write_output_range(attempt, transcript.output, output)
        }
    })?;
    let temporary = scratch.path().join("check-transcript");
    let published = loop {
        let publish = |service: &mut Service| {
            publish_artifact(
                service,
                Publication {
                    attempt,
                    token,
                    name: transcript.name,
                    source: &temporary,
                    relative: transcript.relative,
                    tree: transcript.tree,
                },
            )
        };
        let published = if reclaim {
            service.retry_storage(publish)
        } else {
            publish(service)
        };
        match published {
            Err(error)
                if extend_archive_limit(service, attempt, reclaim, &mut extensions, &error)? => {}
            other => break other,
        }
    };
    published.map_err(transcript_publication_error)?;
    if let Err(error) = scratch.close() {
        note!("Cannot remove check transcript scratch after publication ({error})");
    }
    Ok(paths.state.join(transcript.relative))
}

fn stage_transcript(
    paths: &crate::instance::Paths,
    write: impl FnOnce(&mut std::fs::File) -> Result<()>,
) -> Result<crate::scratch::Scratch> {
    let stage = || -> Result<_> {
        let scratch = crate::scratch::Scratch::new(&paths.cache.join("workers"))?;
        let mut file = std::fs::File::create_new(scratch.path().join("check-transcript"))?;
        write(&mut file)?;
        file.flush()?;
        file.sync_all()?;
        Ok(scratch)
    };
    stage().map_err(|error| with_storage_full_context(error, "Check transcript scratch staging"))
}

fn extend_archive_limit(
    service: &Service,
    attempt: i64,
    reclaim: bool,
    extensions: &mut usize,
    error: &Error,
) -> Result<bool> {
    if reclaim
        && *extensions < 3
        && let Error::Breach(breach) = error
        && service.extend_steadily(attempt, breach.limit, breach.measured)?
    {
        *extensions += 1;
        return Ok(true);
    }
    Ok(false)
}

fn transcript_publication_error(error: Error) -> Error {
    if matches!(&error, Error::Io(error) if error.kind() == io::ErrorKind::StorageFull) {
        return with_storage_full_context(error, "Check transcript retained archive publication");
    }
    let reason = format!(
        "Resource limit: check transcript evidence incomplete: {error}; captured output retained. Raise the limit or free capacity, then resume"
    );
    match error {
        Error::Breach(breach) => {
            Error::Breach(Box::new(crate::resource::Breach { reason, ..*breach }))
        }
        Error::Capacity(refusal) => Error::Capacity(Box::new(crate::resource::StorageRefusal {
            reason,
            ..*refusal
        })),
        _ => Error::Conflict(reason),
    }
}

fn with_storage_full_context(error: Error, operation: &str) -> Error {
    match error {
        Error::Io(error) if error.kind() == io::ErrorKind::StorageFull => io::Error::new(
            error.kind(),
            format!("{operation} ran out of space: {error}"),
        )
        .into(),
        other => other,
    }
}

/// A check passes only when it exits 0 and its own test summaries report no failed test, so a
/// wrapper that hides the test runner's exit status cannot turn failing tests into a pass.
fn verdict(name: &str, code: i32, counts: Option<CheckCounts>) -> Outcome {
    let tally = counts.map_or(String::new(), |c| {
        format!(
            " · {} passed, {} failed, {} ignored",
            c.passed, c.failed, c.ignored
        )
    });
    match counts {
        _ if code != 0 => {
            Outcome::Failed("check", format!("Check {name} failed (exit {code}){tally}"))
        }
        Some(counts) if counts.failed > 0 => Outcome::Failed(
            "check",
            format!("Check {name} exited 0 but its tests failed{tally}"),
        ),
        _ => Outcome::Passed("Check exited 0".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::{Definition, Kind, ModelConfig, VersionedTask},
        instance::Paths,
        scratch::Scratch,
        storage::{Database, OutputReference},
    };
    use std::{fs, os::unix::fs::MetadataExt, path::Path};

    const TREE: &str = "abcdef0123456789abcdef0123456789abcdef01";

    struct Fixture {
        _scratch: Scratch,
        paths: Paths,
        project: PathBuf,
        service: Service,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let scratch = Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
            let base = scratch.path();
            let paths = Paths {
                config: base.join("config"),
                state: base.join("state"),
                cache: base.join("cache"),
                data: base.join("data"),
                runtime: base.join("runtime"),
                executable: std::env::current_exe()?,
            };
            let project = base.join("project");
            fs::create_dir(&project)?;
            let mut service = Service::open(Database::open_in(&paths.state)?, &project, None)?;
            service.set_check("build", "true", 1000)?;
            Ok(Self {
                _scratch: scratch,
                paths,
                project,
                service,
            })
        }

        fn record(
            &mut self,
            code: i32,
            chunks: &[&str],
            known_size: bool,
        ) -> Result<(Run, i64, String)> {
            let definition = Definition {
                title: "Retain the check".into(),
                goal: "Finish the check".into(),
                kind: Kind::Ai,
                category: None,
                lenses: None,
                criteria: vec!["Retain the verdict and proof".into()],
                models: std::array::from_fn(|_| {
                    Some(ModelConfig {
                        provider: "claude".into(),
                        model: "sonnet".into(),
                        effort: None,
                    })
                }),
                attempt_limit: Some(1),
                timeout_ms: None,
                stage_timeout_ms: None,
            };
            let service = &mut self.service;
            let id = service.create(&[definition], None, true)?[0];
            let version = service.current_task()?.unwrap().version;
            let run = service.begin_run(
                VersionedTask { id, version },
                &format!("check-{id}"),
                "base",
                false,
            )?;
            let check = service.checks()?.remove(0);
            let step = service.start_check(&run, &check, "unbound")?;
            for chunk in chunks {
                service.output(&run, "stdout", chunk)?;
            }
            let text = chunks.concat();
            let (outcome, counts) = judge_output("build", code, &text);
            let (passed, report) = match &outcome {
                Outcome::Passed(report) => (true, report),
                Outcome::Failed("check", report) => (false, report),
                _ => panic!("unexpected check verdict"),
            };
            let relative = format!("evidence/attempt-{}/step-{}.log", run.attempt, step.id);
            service.finish_check_with_output(
                &step,
                CheckResult {
                    passed,
                    report,
                    exit: Some(code),
                    counts,
                },
                (&relative, TREE),
                (
                    (0, service.output_cursor(&run)?),
                    known_size.then_some(text.len() as i64),
                ),
                true,
            )?;
            Ok((run, step.id, relative))
        }
    }

    #[test]
    fn interrupted_legacy_sizing_resumes_inside_a_row_and_shares_a_later_log() -> Result<()> {
        let mut f = Fixture::new()?;
        let shared = "the shared check";
        let (original, step, first) = f.record(0, &[shared], true)?;
        f.service
            .finish_run(&original, "succeeded", None, "Done", None)?;
        let stop = AtomicBool::new(false);
        recover_logs(&mut f.service, &f.paths, &stop, 64, || false)?;
        assert!(f.paths.state.join(&first).exists());
        assert_eq!(f.service.transcript_recovery_cursor()?, step);
        limit_transcripts(&mut f.service, 1024)?;

        let (old, oldest, oversized) = f.record(0, &[&"x".repeat(24), &"x".repeat(1200)], false)?;
        f.service
            .finish_run(&old, "succeeded", None, "Done", None)?;
        let (new, later, relative) = f.record(0, &[shared], true)?;
        f.service
            .finish_run(&new, "succeeded", None, "Done", None)?;
        let progress = || -> Result<(Option<i64>, i64, i64)> {
            Ok(Database::open_in(&f.paths.state)?.connection().query_row(
                "SELECT sizing_sequence,sizing_row_bytes,sizing_bytes FROM pending_transcripts WHERE step_id=?1",
                [oldest], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        };
        let mut polls = 0;
        recover_logs(&mut f.service, &f.paths, &stop, 64, || {
            polls += 1;
            if polls == 3 {
                stop.store(true, Ordering::Relaxed);
            }
            stop.load(Ordering::Relaxed)
        })?;
        assert_eq!(progress()?, (Some(2), 0, 24));
        assert_eq!(f.service.transcript_recovery_cursor()?, step);

        drop(f.service);
        let mut service = Service::open(Database::open_in(&f.paths.state)?, &f.project, None)?;
        stop.store(false, Ordering::Relaxed);
        recover_logs(&mut service, &f.paths, &stop, 64, || false)?;
        assert_eq!(progress()?, (Some(2), 64, 88));
        assert_eq!(service.transcript_recovery_cursor()?, oldest);
        assert!(!f.paths.state.join(&relative).exists());

        recover_logs(&mut service, &f.paths, &stop, 64, || false)?;
        assert_eq!(service.transcript_recovery_cursor()?, later);
        assert_eq!(
            fs::metadata(f.paths.state.join(first))?.ino(),
            fs::metadata(f.paths.state.join(relative))?.ino()
        );
        recover_logs(&mut service, &f.paths, &stop, 64, || false)?;
        assert_eq!(progress()?, (Some(2), 128, 152));
        for _ in 0..17 {
            recover_logs(&mut service, &f.paths, &stop, 64, || false)?;
        }
        assert_eq!(service.deferred_check_logs(0)?[0].bytes, Some(1224));
        assert!(!f.paths.state.join(oversized).exists());
        Ok(())
    }

    #[test]
    fn known_oversized_transcript_is_refused_before_scratch_is_created() -> Result<()> {
        let mut f = Fixture::new()?;
        limit_transcripts(&mut f.service, 16)?;
        let text = "x".repeat(64);
        let (run, _, relative) = f.record(0, &[&text], true)?;
        let error = publish_transcript(
            &mut f.service,
            &f.paths,
            (run.attempt, &run.token),
            Transcript {
                relative: &relative,
                name: "build",
                tree: TREE,
                output: (0, 1),
                bytes: Some(64),
            },
            true,
            None,
        )
        .unwrap_err();
        assert!(matches!(error, Error::Breach(breach) if
            breach.limit == crate::resource::Limit::ArtifactFileBytes && breach.measured == 64
        ));
        assert!(!f.paths.cache.join("workers").exists());
        Ok(())
    }

    fn limit_transcripts(service: &mut Service, limit: u64) -> Result<()> {
        use crate::application::resource_limits::{LimitChange, LimitScope};
        service.set_resource_limit(
            LimitScope::Project,
            crate::resource::Limit::ArtifactFileBytes,
            LimitChange {
                value: Some(Some(limit)),
                ceiling: Some(Some(limit)),
            },
            None,
        )?;
        Ok(())
    }

    #[test]
    fn scratch_enospc_keeps_passing_and_failing_verdicts_and_exact_judge_output() -> Result<()> {
        check_storage_full(0, false)?;
        check_storage_full(7, false)
    }

    #[test]
    fn archive_sync_enospc_keeps_verdicts_and_cleans_the_reservation() -> Result<()> {
        check_storage_full(0, true)?;
        check_storage_full(7, true)
    }

    fn check_storage_full(code: i32, archive_full: bool) -> Result<()> {
        let mut f = Fixture::new()?;
        let text = if code == 0 {
            "test result: ok. 1 passed; 0 failed; 0 ignored\n"
        } else {
            "test result: FAILED. 0 passed; 1 failed; 0 ignored\n"
        };
        let (run, step, relative) = f.record(code, &[text], true)?;
        let output = (0, 1);
        let log = handle_recorded_archive(&mut f.service, &run, step, output, |service| {
            if archive_full {
                return publish_transcript_with(
                    service,
                    &f.paths,
                    (run.attempt, &run.token),
                    Transcript {
                        relative: &relative,
                        name: "build",
                        tree: TREE,
                        output,
                        bytes: Some(text.len() as i64),
                    },
                    true,
                    None,
                    |service, publication| {
                        assert_eq!(fs::read(publication.source)?, text.as_bytes());
                        service.publish_artifact_with_sync(publication, |file| {
                            assert_eq!(file.metadata()?.len(), text.len() as u64);
                            Err(io::Error::from_raw_os_error(nix::libc::ENOSPC))
                        })
                    },
                );
            }
            stage_transcript(&f.paths, |file| {
                file.write_all(b"partial scratch copy")?;
                let full = io::Error::from_raw_os_error(nix::libc::ENOSPC);
                let error = if code == 0 {
                    full
                } else {
                    // Temporary directory creation can wrap the errno with path context.
                    io::Error::new(full.kind(), format!("{full} at transcript scratch"))
                };
                Err(error.into())
            })?;
            unreachable!()
        })?;
        assert!(matches!(log, CheckLog::Pending { .. }));
        assert_no_transcript_scratch(&f.paths.cache)?;

        let check = f.service.checks()?.remove(0);
        let result = f
            .service
            .pending_transcript(&run, &check, TREE, "unbound")?
            .unwrap();
        assert_eq!(result.bytes, Some(text.len() as i64));
        assert_eq!(result.passed, code == 0);
        assert_eq!(
            result.counts,
            counts(i64::from(code == 0), i64::from(code != 0), 0)
        );
        let packet = f.paths.state.join("packet");
        f.service
            .review_bundle(run.attempt, &f.paths.state, &packet, TREE, "", false)?;
        let operation = if archive_full {
            "retained archive publication"
        } else {
            "scratch staging"
        };
        let index = fs::read_to_string(packet.join("steps.jsonl"))?;
        assert!(
            index.contains(&format!("{operation} ran out of space")),
            "{index}"
        );
        let reference: OutputReference = serde_json::from_slice(&fs::read(
            packet.join(format!("check-output-{step}.ktask-output")),
        )?)
        .map_err(io::Error::other)?;
        assert_eq!(reference.read(0, 4096)?, text.as_bytes());
        assert!(!f.paths.state.join(&relative).exists());
        assert!(!f.paths.state.join(format!("{relative}.pending")).exists());
        let db = Database::open_in(&f.paths.state)?;
        let reservations = || {
            db.connection()
                .query_row("SELECT count(*) FROM artifact_reservations", [], |row| {
                    row.get::<_, i64>(0)
                })
        };
        assert_eq!(reservations()?, 0);

        f.service.finish_run(
            &run,
            if code == 0 { "succeeded" } else { "failed" },
            (code != 0).then_some("check"),
            "Recorded verdict",
            None,
        )?;
        recover_logs(
            &mut f.service,
            &f.paths,
            &AtomicBool::new(false),
            128,
            || false,
        )?;
        assert_eq!(fs::read(f.paths.state.join(relative))?, text.as_bytes());
        recover_logs(
            &mut f.service,
            &f.paths,
            &AtomicBool::new(false),
            128,
            || false,
        )?;
        assert_eq!(reservations()?, 0);
        let archived: i64 = db.connection().query_row(
            "SELECT count(*) FROM artifacts WHERE attempt_id=?1",
            [run.attempt],
            |row| row.get(0),
        )?;
        assert_eq!(archived, 1);
        assert_eq!(reference.read(0, 4096)?, text.as_bytes());
        let checks: i64 = db.connection().query_row(
            "SELECT count(*) FROM attempt_steps WHERE role='check' AND exit_code=?1",
            [code],
            |row| row.get(0),
        )?;
        assert_eq!(checks, 1);
        assert_no_transcript_scratch(&f.paths.cache)?;
        Ok(())
    }

    fn assert_no_transcript_scratch(cache: &Path) -> Result<()> {
        for entry in fs::read_dir(cache.join("workers"))? {
            assert!(!entry?.file_type()?.is_dir());
        }
        Ok(())
    }

    fn counts(passed: i64, failed: i64, ignored: i64) -> Option<CheckCounts> {
        Some(CheckCounts {
            passed,
            failed,
            ignored,
        })
    }

    #[test]
    fn any_reported_failure_fails_the_check() {
        let failed =
            |code, counts| matches!(verdict("build", code, counts), Outcome::Failed("check", _));
        assert!(
            failed(0, counts(7, 1, 1)),
            "exit 0 cannot hide a failed test"
        );
        assert!(failed(2, counts(1, 0, 0)) && failed(2, None));
        assert!(!failed(0, counts(1, 0, 0)) && !failed(0, None));
    }
}
