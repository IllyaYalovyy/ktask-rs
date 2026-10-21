use super::{Context, Outcome, execute, operational_backoff, process::Invocation, provider};
use crate::{
    application::{
        CheckCounts, Service,
        execution::{Check, Run},
        require,
    },
    storage::Result,
};
use std::{io::Write, path::PathBuf};

pub(super) fn run(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    checks: &[Check],
) -> Result<Outcome> {
    let mut evidence = String::new();
    for check in checks {
        for recheck in 0..=2 {
            let (outcome, path, counts) = run_one(service, run, ctx, check)?;
            match outcome {
                Outcome::Passed(_) => {
                    evidence.push_str(&format!(
                        "\nSupervisor check evidence: {}",
                        serde_json::json!({
                            "name": check.name, "command": check.command, "exit_code": 0,
                            "transcript": path, "current_counts": counts,
                            "previous_checkpoint_counts": service.previous_check_counts(run, check.id)?,
                        })
                    ));
                    break;
                }
                Outcome::Failed(category, message) => {
                    let message = format!("{message}; supervisor transcript: {}", path.display());
                    if category == "environment" && recheck < 2 {
                        service.output(run, "system", &format!(
                            "Check infrastructure failed · automatic retry {}/2 after {}s · transcript retained; Pause cancels",
                            recheck + 1, 1 << recheck,
                        ))?;
                        if let Some(outcome) = operational_backoff(service, run, ctx, recheck)? {
                            return Ok(outcome);
                        }
                        continue;
                    }
                    if category == "check" && recheck == 0 && executable_busy(&path)? {
                        evidence.push_str(&format!("\nEarlier supervisor check failed with executable contention: {message}. One verification retry follows, without another AI worker. This does not prove the root cause is fixed."));
                        service.output(run, "system", "Check encountered ETXTBSY (executable busy): verification retry 1/1 after 1s in the same attempt; original failure and transcript retained; Pause cancels")?;
                        if let Some(outcome) = operational_backoff(service, run, ctx, 0)? {
                            return Ok(outcome);
                        }
                        continue;
                    }
                    return Ok(Outcome::Failed(category, message));
                }
                other => return Ok(other),
            }
        }
    }
    Ok(Outcome::Passed(evidence))
}

fn executable_busy(path: &std::path::Path) -> Result<bool> {
    use std::io::BufRead;
    let transcript = std::io::BufReader::new(std::fs::File::open(path)?);
    for line in transcript.lines() {
        let line = line?;
        if (line.contains("code: 26") && line.contains("kind: ExecutableFileBusy"))
            || line.contains("[Errno 26] Text file busy")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn run_one(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    check: &Check,
) -> Result<(Outcome, PathBuf, Option<CheckCounts>)> {
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
        env: Vec::new(),
        cwd,
        prompt: String::new(),
        schema: None,
        scratch_root: ctx.paths.cache.join("workers"),
        worker_lock: ctx.workspace.worker_lock.clone(),
        evidence_pin_guardian: None,
        callback: None,
    };
    let mut stream = provider::Stream::default();
    let cursor = service.output_cursor(run)?;
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
    if let Ok(Outcome::Failed("environment", message)) = &result {
        service.finish_step(&step, false, message, None)?;
    }
    let relative = format!("evidence/attempt-{}/step-{}.log", run.attempt, step.id);
    let path = ctx.paths.state.join(&relative);
    std::fs::create_dir_all(path.parent().expect("evidence directory"))?;
    let mut transcript = std::fs::File::create_new(&path)?;
    service.write_output_since(run, cursor, &mut transcript)?;
    transcript.flush()?;
    transcript.sync_all()?;
    service.record_artifact(
        run,
        &check.name,
        &relative,
        transcript.metadata()?.len() as i64,
    )?;
    let counts = if matches!(
        &result,
        Ok(Outcome::Passed(_) | Outcome::Failed("check", _))
    ) {
        parse_counts(&std::fs::read_to_string(&path)?)
    } else {
        None
    };
    if let Some(counts) = counts {
        service.record_check_counts(&step, counts)?;
        service.output(
            run,
            "system",
            &format!(
                "Check {}: {} passed, {} failed, {} ignored",
                check.name, counts.passed, counts.failed, counts.ignored
            ),
        )?;
    }
    Ok((result?, path, counts))
}

/// Sum completed Rust test harness suites in the check's captured output.
fn parse_counts(transcript: &str) -> Option<CheckCounts> {
    let mut total: Option<CheckCounts> = None;
    for line in transcript.lines() {
        let Some(summary) = line.trim().strip_prefix("test result: ") else {
            continue;
        };
        let mut counts = [None; 3];
        for segment in summary.split(';') {
            let words = segment.split_whitespace().collect::<Vec<_>>();
            let Some([number, kind]) = words
                .windows(2)
                .find(|pair| matches!(pair[1], "passed" | "failed" | "ignored"))
            else {
                continue;
            };
            let index = match *kind {
                "passed" => 0,
                "failed" => 1,
                "ignored" => 2,
                _ => continue,
            };
            counts[index] = number.parse::<i64>().ok().filter(|n| *n >= 0);
        }
        if let [Some(passed), Some(failed), Some(ignored)] = counts {
            let next = CheckCounts {
                passed,
                failed,
                ignored,
            };
            total = Some(match total {
                Some(prior) => CheckCounts {
                    passed: prior.passed.checked_add(next.passed)?,
                    failed: prior.failed.checked_add(next.failed)?,
                    ignored: prior.ignored.checked_add(next.ignored)?,
                },
                None => next,
            });
        }
    }
    total
}
