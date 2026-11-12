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
            Access::Write => None,
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
    let configured = agent::model_role(role);
    let model = role_model(run, configured).expect("preflight models");
    let turn = Turn {
        role: configured,
        model,
        provider: Provider::of(model)?,
    };
    let scope = turn.provider.auth_scope()?;
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
    let packet_context = service.review_bundle(
        run.attempt,
        &ctx.paths.state,
        packet.path(),
        &tree,
        evidence,
    )?;
    let packet_context = format!(
        "{packet_context}\nRead/search without opening the database: '{}' evidence FILE --offset 0 --limit 4096 [--search TEXT]. Use next_offset until eof. This command works in the read-only sandbox.\n",
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
        return Ok(Outcome::Question(question));
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
        if access == Access::Write {
            self.provider.writable_network(network)?;
        }
        let launch = self.provider.launch(
            self.model,
            step.provider_session.as_deref(),
            access,
            network,
            &agent::schema(),
            prompt,
        );
        let mut invocation = provider_invocation(
            self.provider,
            launch,
            access,
            &ctx.workspace.root,
            ctx.paths,
            ctx.scratch_root(),
            ctx.workspace.worker_lock.clone(),
        )?;
        invocation.callback = (access == Access::Write).then(|| callback::Capability {
            project: ctx.workspace.root.clone(),
            state: ctx.paths.state.clone(),
            task: run.task,
            attempt: run.attempt,
            token: run.token.clone(),
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
