//! The read-only judgment shared by every diagnosis: a check timeout and a storage-limit
//! breach reach the remediation model the same way and get the same three answers.
use super::{
    Context, Outcome,
    backoff::{self, Notice},
    cycle,
    judge::read_only_turn,
    turn::operational_failure,
};
use crate::{
    application::{
        Service,
        execution::Run,
        progress::{BASELINE_FAILURE_NOTE, Phase, Progress},
        prompt::{self, Turn},
        stop::CHECK_TIMEOUT_DIAGNOSIS_LIMIT,
    },
    storage::Result,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Judgment {
    Repair { reason: String, plan: String },
    Extend { reason: String },
    Escalate { reason: String },
}

impl Judgment {
    pub(super) fn reason(&self) -> &str {
        match self {
            Self::Repair { reason, .. } | Self::Extend { reason } | Self::Escalate { reason } => {
                reason.trim()
            }
        }
    }
}

/// What to ask the remediation model and how to name the turn.
pub(super) struct Inquiry<'a> {
    pub turn: Turn,
    /// The case: progress, the failure report, what the judge may decide and the answers.
    pub context: &'a str,
    /// The step's scope and the label in the output.
    pub names: (&'a str, &'a str),
    pub notice: Notice,
    /// What is diagnosed, as the owner reads it when the diagnosis cannot run.
    pub subject: &'static str,
    /// Appended to the owner's text when the diagnosis runs out of stage time.
    pub report: &'a str,
    pub repair_available: bool,
    pub extension_available: bool,
}

/// How a diagnosis ended.
pub(super) enum Diagnosed {
    Judgment(Judgment),
    /// The attempt ends or pauses with this outcome instead.
    Stop(Outcome),
}

/// Prepares the evidence, runs the read-only judgment turn with its bounded retries and
/// maps whatever else happened to the outcome that ends the attempt.
pub(super) fn diagnose(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    inquiry: &Inquiry<'_>,
) -> Result<Diagnosed> {
    let packet = crate::scratch::Scratch::new(&ctx.paths.state.join("review-packets"))?;
    let tree = ctx.workspace.fingerprint(&ctx.scratch_root())?;
    let evidence = service.review_bundle(
        run.attempt,
        &ctx.paths.state,
        packet.path(),
        &tree,
        inquiry.context,
    )?;
    let evidence = format!(
        "{}\n{evidence}\nResolved project tools: {}. Environment fingerprint: {}.\nRead/search with '{}' evidence FILE --offset 0 --limit 4096 [--search TEXT]; follow next_offset.",
        inquiry.context,
        ctx.tools.description,
        ctx.tools.fingerprint,
        ctx.paths.executable.display()
    );
    let prompt = prompt::compose(
        inquiry.turn,
        &run.effective_category(),
        &prompt::Task {
            title: &run.definition.title,
            goal: &run.definition.goal,
            criteria: &run.definition.criteria,
        },
        &evidence,
        None,
    )?
    .text();
    let schema = schema(inquiry.repair_available, inquiry.extension_available);
    let outcome = backoff::bounded(
        service,
        run,
        ctx,
        inquiry.notice,
        operational_failure,
        |service, ctx| judge_once(service, run, ctx, &prompt, &schema, inquiry.names),
    )?;
    if let Some(outcome) = ctx.deadline_outcome(run) {
        return Ok(Diagnosed::Stop(outcome));
    }
    Ok(match outcome {
        Outcome::Diagnosis(judgment) => Diagnosed::Judgment(judgment),
        Outcome::Failed("timeout", message) => Diagnosed::Stop(Outcome::Failed(
            "timeout",
            format!(
                "{CHECK_TIMEOUT_DIAGNOSIS_LIMIT}: {message}\n{}",
                inquiry.report
            ),
        )),
        Outcome::Failed("configuration" | "agent", _) => Diagnosed::Stop(outcome),
        Outcome::Failed(category, message) => Diagnosed::Stop(Outcome::DiagnosisUnavailable(
            inquiry.subject,
            category,
            message,
        )),
        other => Diagnosed::Stop(other),
    })
}

/// Sends the repair plan to the executing worker as the next repair round.
pub(super) fn apply_repair(
    service: &Service,
    run: &Run,
    progress: &mut Progress,
    reason: &str,
    report: &str,
) -> Result<()> {
    let baseline = progress.phase == Phase::Baseline;
    cycle::fix_checks(service, run, progress, report)?;
    if baseline {
        progress.context.push('\n');
        progress.context.push_str(BASELINE_FAILURE_NOTE);
    }
    progress.why = reason.lines().next().unwrap_or_default().to_owned();
    Ok(())
}

pub(super) fn schema(repair_available: bool, extension_available: bool) -> Value {
    let choice = |action| {
        json!({
            "type":"object","additionalProperties":false,"required":["action","reason"],
            "properties":{"action":{"type":"string","enum":[action]},"reason":{"type":"string"}}
        })
    };
    let mut alternatives = vec![choice("escalate")];
    if extension_available {
        alternatives.push(choice("extend"));
    }
    if repair_available {
        alternatives.push(json!({
            "type":"object","additionalProperties":false,"required":["action","reason","plan"],
            "properties":{"action":{"type":"string","enum":["repair"]},"reason":{"type":"string"},"plan":{"type":"string"}}
        }));
    }
    // A lone alternative is already an object schema.
    let judgment = if alternatives.len() == 1 {
        alternatives.remove(0)
    } else {
        json!({"anyOf":alternatives})
    };
    json!({"type":"object","additionalProperties":false,"required":["judgment"],"properties":{"judgment":judgment}})
}

/// One read-only judgment turn. `(scope, label)` name it in the step and the output.
pub(super) fn judge_once(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    prompt: &str,
    schema: &Value,
    names: (&str, &str),
) -> Result<Outcome> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Response {
        judgment: Judgment,
    }

    let (step, stream, outcome) = read_only_turn(
        service,
        run,
        ctx,
        names,
        (prompt.to_owned(), schema.clone()),
    )?;
    if let Some(outcome) = outcome {
        return Ok(outcome);
    }
    if stream.completed
        && !stream.failed
        && let Ok(response) =
            serde_json::from_str::<Response>(stream.last_message.as_deref().unwrap_or(""))
        && !response.judgment.reason().is_empty()
        && !matches!(&response.judgment, Judgment::Repair { plan, .. } if plan.trim().is_empty())
    {
        service.finish_step(&step, true, response.judgment.reason())?;
        return Ok(Outcome::Diagnosis(response.judgment));
    }
    let message = format!("{} did not return a complete decision", names.1);
    service.finish_step(&step, false, &message)?;
    Ok(Outcome::Failed("protocol", message))
}
