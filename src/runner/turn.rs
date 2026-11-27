//! AI turns of the attempt's workers and reviewers: one provider process per turn, its
//! structured assessment recorded as the step's result.
use super::{
    Context, Outcome, agent,
    backoff::{self, Notice},
    callback,
    lens::LensTurn,
    process::Invocation,
    provider::{self, Access, Provider},
    sandbox,
    worker_io::{Finish, execute},
};
use crate::{
    application::{
        ModelConfig, ROLES, Service,
        execution::{Run, Step},
        has_golden_screens,
        lenses::Lens,
    },
    instance::Paths,
    storage::Result,
};
use std::path::{Path, PathBuf};

/// The worker for one provider turn in `project`. A read-only turn runs in the sandbox, so
/// whatever its provider allows, it cannot change the project or ktask's own records.
pub(crate) fn provider_invocation(
    provider: Provider,
    mut launch: provider::Launch,
    access: Access,
    project: &Path,
    paths: &Paths,
    scratch_root: PathBuf,
    worker_lock: PathBuf,
) -> Result<Invocation> {
    if access == Access::ReadOnly {
        launch.allow_evidence(provider, &paths.executable)?;
    }
    Ok(Invocation {
        env: launch.env,
        prompt: launch.prompt,
        schema: launch.schema_file,
        sandbox: match access {
            Access::ReadOnly => Some(sandbox::Sandbox::for_project(project, paths)?),
            // A fix turn's sandbox is the caller's own.
            Access::Write | Access::Repair => None,
        },
        ..Invocation::new(
            provider.executable()?,
            launch.args,
            project.to_owned(),
            scratch_root,
            worker_lock,
        )
    })
}

fn protect_installations(
    invocation: &mut Invocation,
    access: Access,
    policy: &sandbox::WorkspaceWrite<'_>,
    installations: &[PathBuf],
) -> Result<()> {
    if access == Access::Write {
        if !installations.is_empty() {
            invocation.sandbox = Some(policy.provider_mount()?);
        }
    } else if let Some(mount) = invocation.sandbox.as_mut() {
        for path in installations {
            mount.protect(path)?;
        }
    }
    Ok(())
}

/// The model configured for `role`, one of [`ROLES`].
pub(super) fn role_model<'r>(run: &'r Run, role: &str) -> Option<&'r ModelConfig> {
    ROLES
        .iter()
        .position(|candidate| *candidate == role)
        .and_then(|index| run.definition.models[index].as_ref())
}

/// A turn of `role`, retried automatically after an operational provider failure.
pub(super) fn ai(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    role: &'static str,
    evidence: &str,
) -> Result<Outcome> {
    backoff::bounded(
        service,
        run,
        ctx,
        Notice {
            failed: "Provider turn failed",
            retained: "same attempt, work retained",
        },
        operational_failure,
        |service, ctx| ai_turn(service, run, ctx, role, evidence, None, None),
    )
}

/// A review turn through one lens, retried like any other after an operational failure.
pub(super) fn ai_lens(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    evidence: &str,
    lens: &LensTurn<'_>,
) -> Result<Outcome> {
    backoff::bounded(
        service,
        run,
        ctx,
        Notice {
            failed: "Provider turn failed",
            retained: "same attempt, work retained",
        },
        operational_failure,
        |service, ctx| ai_turn(service, run, ctx, "review", evidence, None, Some(lens)),
    )
}

/// A failure of the provider or its environment, not of the work: retrying may help.
pub(super) fn operational_failure(outcome: &Outcome) -> bool {
    matches!(
        outcome,
        Outcome::Failed("transport" | "protocol" | "environment", _)
    )
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
    lens: Option<&LensTurn<'_>>,
) -> Result<Outcome> {
    if let Some(wait) = ctx.environment_wait(service, run, "before the provider turn")? {
        return Ok(wait);
    }
    let configured = agent::model_role(role);
    let model = role_model(run, configured).expect("preflight models");
    let turn = Turn {
        role: configured,
        model,
        provider: Provider::of(model)?,
    };
    let scope = format!(
        "{}|environment:{}",
        turn.provider.auth_scope()?,
        ctx.tools.fingerprint
    );
    // A lens review starts its own session; its nudge continues that same session.
    let step = match lens.filter(|_| nudge.is_none()) {
        Some(_) => service.start_independent_step(run, configured, model, &scope)?,
        None => service.start_ai_step(run, configured, model, &scope, nudge.is_some())?,
    };
    service.output(
        run,
        "system",
        &format!(
            "{}: {}/{} · {}",
            lens.map_or_else(
                || role.to_owned(),
                |l| format!("{role} ({} lens)", l.lens.name)
            ),
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
    let ask = Ask {
        role,
        evidence,
        nudge,
        lens,
    };
    let outcome = answered_turn(service, run, ctx, &ask, &turn, &step);
    if lens.is_some() {
        // However it ended, no later turn resumes a lens review's session.
        let closed = service.close_session(&step);
        outcome.and_then(|outcome| closed.map(|()| outcome))
    } else {
        outcome
    }
}

/// What a turn is asked: the role it plays, the evidence it reads, the question it was
/// nudged with and the lens it reviews through.
struct Ask<'a> {
    role: &'static str,
    evidence: &'a str,
    nudge: Option<&'a str>,
    lens: Option<&'a LensTurn<'a>>,
}

/// The provider process and the recorded verdict of a turn whose step has started.
fn answered_turn(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    ask: &Ask<'_>,
    turn: &Turn<'_>,
    step: &Step,
) -> Result<Outcome> {
    let Ask {
        role,
        evidence,
        nudge,
        lens,
    } = *ask;
    let configured = turn.role;
    let packet = crate::scratch::Scratch::new(&ctx.paths.state.join("review-packets"))?;
    let tree = ctx.workspace.fingerprint(&ctx.scratch_root())?;
    // Only review and validation follow a passed check, whose screen comparison they rely on.
    let golden_screens = matches!(role, "review" | "validation")
        && run.runs_checks()
        && has_golden_screens(&ctx.workspace.root);
    let packet_context = service.review_bundle(
        run.attempt,
        &ctx.paths.state,
        packet.path(),
        &tree,
        evidence,
        golden_screens,
    )?;
    let packet_context = format!(
        "{packet_context}\nResolved project tools: {}. Environment fingerprint: {}.\nRead/search without opening the database: '{}' evidence FILE --offset 0 --limit 4096 [--search TEXT]. Use next_offset until eof. This command works in the read-only sandbox.\n",
        ctx.tools.description,
        ctx.tools.fingerprint,
        ctx.paths.executable.display()
    );
    let prompt = match (nudge, lens) {
        (Some(question), _) => agent::nudge(role, question),
        (None, Some(lens)) => agent::lens_prompt(run, lens.lens, &packet_context)?,
        (None, None) => agent::prompt(
            run,
            role,
            &packet_context,
            &ctx.paths.executable,
            &ctx.workspace.root,
            service.scratch_limits(run.attempt)?,
        )?,
    };
    let invocation = turn.invocation(service, run, ctx, step, prompt)?;
    let mut stream = provider::Stream::new(turn.provider);
    if let Some(outcome) = provider_turn(
        service,
        run,
        step,
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
            service.finish_step(step, false, &e.to_string())?;
            return Ok(Outcome::Failed("protocol", e.to_string()));
        }
    };
    if role == "validation" && run.runs_checks() {
        add_check_regressions(service, run, &mut assessment)?;
    }
    if assessment.outcome == "blocked" {
        service.finish_assessment(step, false, &assessment.summary)?;
        let question = assessment.question.expect("validated");
        if nudge.is_none() {
            return ai_turn(service, run, ctx, role, evidence, Some(&question), lens);
        }
        if matches!(role, "review" | "validation") {
            return Ok(Outcome::Failed(
                "protocol",
                format!("{role} asked instead of deciding from the evidence: {question}"),
            ));
        }
        return Ok(Outcome::WorkerQuestion(question));
    }
    let Some(lens) = lens else {
        return record_assessment(service, run, step, role, assessment, None);
    };
    let review = lens.review(step, &assessment);
    let outcome = record_assessment(service, run, step, role, assessment, Some(lens.lens))?;
    service.record_lens_review(run, lens.tree, &review)?;
    Ok(outcome)
}

/// Who answers a turn: the configured role and its model.
struct Turn<'r> {
    role: &'static str,
    model: &'r ModelConfig,
    provider: Provider,
}

impl Turn<'_> {
    /// The provider process for this turn: only execution writes, with the callback
    /// capability.
    fn invocation(
        &self,
        service: &Service,
        run: &Run,
        ctx: &Context<'_>,
        step: &Step,
        prompt: String,
    ) -> Result<Invocation> {
        let access = Access::for_role(self.role);
        let network = access == Access::Write && service.agent_network()?;
        let policy = sandbox::WorkspaceWrite::new(
            &ctx.workspace.root,
            &ctx.tools.writable_roots,
            &ctx.tools.installations,
            network,
        );
        if access == Access::Write {
            self.provider.writable_network(network)?;
        }
        let mut launch = self.provider.launch(
            self.model,
            step.provider_session.as_deref(),
            access,
            network,
            &agent::schema(),
            prompt,
        );
        if access == Access::Write
            && self.provider == Provider::Codex
            && !policy.codex_roots().is_empty()
        {
            let position = launch
                .args
                .iter()
                .position(|arg| arg == "resume")
                .unwrap_or(launch.args.len() - 1);
            launch.args.splice(
                position..position,
                [
                    "-c".into(),
                    format!(
                        "sandbox_workspace_write.writable_roots={}",
                        serde_json::to_string(policy.codex_roots()).expect("root JSON")
                    ),
                ],
            );
        }
        let mut invocation = provider_invocation(
            self.provider,
            launch,
            access,
            &ctx.workspace.root,
            ctx.paths,
            ctx.scratch_root(),
            ctx.workspace.worker_lock.clone(),
        )?;
        if access == Access::Write {
            invocation.env.extend(ctx.tools.env.clone());
            if let Some((_, wrappers)) = ctx
                .tools
                .env
                .iter()
                .find(|(key, _)| key == "KTASK_ANDROID_WRAPPER_DIR")
                && let Some((_, path)) = invocation.env.iter_mut().find(|(key, _)| key == "PATH")
            {
                *path = format!("{wrappers}:{path}");
            }
        }
        protect_installations(&mut invocation, access, &policy, &ctx.tools.installations)?;
        invocation.callback = (access == Access::Write).then(|| callback::Capability {
            project: ctx.workspace.root.clone(),
            state: ctx.paths.state.clone(),
            task: run.task,
            attempt: run.attempt,
            token: run.token.clone(),
            environment_fingerprint: ctx.tools.fingerprint.clone(),
            environment_path: ctx
                .tools
                .env
                .iter()
                .find(|(key, _)| key == "PATH")
                .map_or_else(
                    || std::env::var("PATH").unwrap_or_default(),
                    |(_, value)| value.clone(),
                ),
            fix: None,
        });
        Ok(invocation)
    }
}

/// A supervisor check that regressed since the last checkpoint is a critical finding, even
/// when validation missed it.
fn add_check_regressions(
    service: &Service,
    run: &Run,
    assessment: &mut agent::Assessment,
) -> Result<()> {
    for reason in service.check_regressions(run)? {
        assessment.findings.push(agent::Finding {
            severity: agent::Severity::Critical,
            description: reason,
        });
    }
    if assessment
        .findings
        .iter()
        .any(|finding| finding.severity == agent::Severity::Critical)
    {
        assessment.outcome = "changes_requested".into();
    }
    Ok(())
}

/// Records a decided assessment: criteria, findings and whether the work needs another round.
fn record_assessment(
    service: &mut Service,
    run: &Run,
    step: &Step,
    role: &str,
    assessment: agent::Assessment,
    lens: Option<&Lens>,
) -> Result<Outcome> {
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
            step,
            criterion.ordinal,
            criterion.passed,
            &criterion.evidence,
        )?;
    }
    let mut summary = match lens {
        Some(lens) => format!("{} lens: {}", lens.name, assessment.summary),
        None => assessment.summary.clone(),
    };
    for finding in &assessment.findings {
        let severity = if finding.severity == agent::Severity::Critical {
            "Critical"
        } else {
            "Minor"
        };
        summary.push_str(&format!("\n{severity}: {}", finding.description));
    }
    service.finish_assessment(step, needs_work, &summary)?;
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
pub(super) fn provider_turn(
    service: &mut Service,
    run: &Run,
    step: &Step,
    ctx: &mut Context<'_>,
    spec: &Invocation,
    stream: &mut provider::Stream,
    role: &str,
) -> Result<Option<Outcome>> {
    let timeout = run.definition.stage_timeout_ms;
    let finish = execute(
        service,
        run,
        step,
        ctx,
        spec,
        timeout,
        Some((&mut *stream, role)),
    )?;
    if let Some(reason) = service.pending_dependency(run.attempt)? {
        service.finish_step(step, false, &reason)?;
        return Ok(Some(Outcome::Dependency(reason)));
    }
    let outcome = match finish {
        Finish::Exited(_) if stream.rate_limited => Outcome::RateLimit(stream.reset_at_ms),
        Finish::Exited(0) if stream.configuration_error.is_none() => return Ok(None),
        Finish::Exited(code) => exit_failure(run, stream, role, code),
        Finish::Stopped(outcome) => outcome,
    };
    if let Outcome::Failed(_, message) = &outcome {
        service.finish_step(step, false, message)?;
    }
    Ok(Some(outcome))
}

/// Why a provider that exited with `code` produced no usable turn.
fn exit_failure(run: &Run, stream: &provider::Stream, role: &str, code: i32) -> Outcome {
    let provider = stream.provider.label();
    match &stream.configuration_error {
        Some((setting, detail)) => Outcome::Failed(
            "configuration",
            format!(
                "{role} {setting} rejected by {provider} for {} (exit {code}): {detail}. Fix the {setting} setting before retrying",
                role_model(run, role)
                    .map(|model| format!("{}/{}", model.provider, model.model))
                    .unwrap_or_else(|| "configured model".into()),
            ),
        ),
        None => Outcome::Failed(
            "transport",
            match stream.error_summary() {
                Some(error) => format!("{provider} exited {code}: {error}"),
                None => {
                    format!("{provider} exited {code} without an error message; see stored output")
                }
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, net::TcpListener};

    #[test]
    fn android_provider_launch_keeps_api_transport_and_denies_agent_host_transport() -> Result<()> {
        let root = tempfile::tempdir_in(std::env::temp_dir())?;
        let sdk = root.path().join("sdk");
        let scratch = root.path().join("scratch");
        fs::create_dir_all(sdk.join("platform-tools"))?;
        fs::create_dir_all(&scratch)?;
        fs::write(sdk.join("platform-tools/adb"), b"fixture")?;
        let sentinel = TcpListener::bind("127.0.0.1:0")?;
        sentinel.set_nonblocking(true)?;
        let port = sentinel.local_addr()?.port();
        let paths = Paths {
            config: root.path().join("config"),
            state: root.path().join("state"),
            cache: root.path().join("cache"),
            data: root.path().join("data"),
            runtime: root.path().join("runtime"),
            executable: std::env::current_exe()?,
        };
        let mut invocation = provider_invocation(
            Provider::Codex,
            provider::Launch {
                args: vec![],
                env: vec![],
                schema_file: None,
                prompt: String::new(),
            },
            Access::Write,
            root.path(),
            &paths,
            scratch.clone(),
            root.path().join("worker.lock"),
        )?;
        let installations = vec![sdk];
        let policy = sandbox::WorkspaceWrite::new(root.path(), &[], &installations, false);
        protect_installations(&mut invocation, Access::Write, &policy, &installations)?;
        invocation.executable = "/bin/bash".into();
        invocation.args = vec!["-c".into(), format!("echo api >/dev/tcp/127.0.0.1/{port}")];
        let provider = invocation.sandbox.expect("provider sandbox");
        assert!(
            provider
                .command(&invocation.executable, &scratch)
                .args(&invocation.args)
                .status()?
                .success()
        );
        assert!(
            sentinel.accept().is_ok(),
            "provider could not reach API sentinel"
        );
        let child = policy.probe_mount()?;
        assert!(
            !child
                .command(&invocation.executable, &scratch)
                .args(&invocation.args)
                .status()?
                .success()
        );
        assert!(
            sentinel.accept().is_err(),
            "agent child reached unrelated host sentinel"
        );
        Ok(())
    }
}
