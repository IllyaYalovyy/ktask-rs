use super::provider::{Access, Provider, Stream};
use crate::{
    application::{
        ModelConfig,
        categories::Category,
        execution::Run,
        lenses::Lens,
        prompt::{self, Callback, Turn, compose},
    },
    storage::{Error, Result},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Assessment {
    pub outcome: String,
    pub summary: String,
    pub question: Option<String>,
    pub criteria: Vec<Evidence>,
    #[serde(default)]
    pub findings: Vec<Finding>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub severity: Severity,
    pub description: String,
}
#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    Minor,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub ordinal: i64,
    pub passed: bool,
    pub evidence: String,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepairTask {
    pub title: String,
    pub goal: String,
    pub criteria: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct ModelOverrides(pub Option<std::collections::BTreeMap<String, Option<ModelConfig>>>);

impl<'de> Deserialize<'de> for ModelOverrides {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let models =
            Option::<std::collections::BTreeMap<String, Option<ModelConfig>>>::deserialize(
                deserializer,
            )?;
        if let Some(models) = &models
            && (models.len() != crate::application::ROLES.len()
                || models
                    .keys()
                    .any(|role| !crate::application::ROLES.contains(&role.as_str())))
        {
            return Err(serde::de::Error::custom(
                "revise models must contain execution, review, validation and remediation roles",
            ));
        }
        Ok(Self(models))
    }
}

fn deserialize_model_overrides<'de, D>(
    deserializer: D,
) -> std::result::Result<ModelOverrides, D::Error>
where
    D: serde::Deserializer<'de>,
{
    ModelOverrides::deserialize(deserializer)
}

impl ModelOverrides {
    pub fn effective(&self) -> impl Iterator<Item = (&String, &ModelConfig)> {
        self.0
            .iter()
            .flat_map(|models| models.iter())
            .filter_map(|(role, model)| model.as_ref().map(|model| (role, model)))
    }
}

/// Verdict of the failure judge that opens every repair attempt.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Judgment {
    /// Keep the task; the next worker turn follows this concrete plan.
    Repair { plan: String, reason: String },
    /// The task itself is wrong, unclear or too big: replace its definition.
    Revise {
        title: String,
        goal: String,
        criteria: Vec<String>,
        #[serde(deserialize_with = "deserialize_model_overrides")]
        models: ModelOverrides,
        reason: String,
    },
    /// Prerequisite work is missing: ordinary tasks go before this one.
    Insert {
        tasks: Vec<RepairTask>,
        reason: String,
    },
    /// A product decision only the owner can make.
    Ask { question: String, reason: String },
}

pub fn judge_schema() -> Value {
    let task = json!({"type":"object","additionalProperties":false,"required":["title","goal","criteria"],"properties":{"title":{"type":"string"},"goal":{"type":"string"},"criteria":{"type":"array","minItems":1,"items":{"type":"string"}}}});
    let model = json!({"type":"object","additionalProperties":false,"required":["provider","model","effort"],"properties":{"provider":{"type":"string"},"model":{"type":"string"},"effort":{"type":["string","null"]}}});
    let models = json!({"anyOf":[{"type":"null"},{"type":"object","additionalProperties":false,"required":["execution","review","validation","remediation"],"properties":{"execution":{"anyOf":[model,{"type":"null"}]},"review":{"anyOf":[model,{"type":"null"}]},"validation":{"anyOf":[model,{"type":"null"}]},"remediation":{"anyOf":[model,{"type":"null"}]}}}]});
    json!({"type":"object","additionalProperties":false,"required":["judgment"],"properties":{"judgment":{"anyOf":[
        {"type":"object","additionalProperties":false,"required":["action","plan","reason"],"properties":{"action":{"type":"string","enum":["repair"]},"plan":{"type":"string"},"reason":{"type":"string"}}},
        {"type":"object","additionalProperties":false,"required":["action","title","goal","criteria","models","reason"],"properties":{"action":{"type":"string","enum":["revise"]},"title":{"type":"string"},"goal":{"type":"string"},"criteria":{"type":"array","minItems":1,"items":{"type":"string"}},"models":models,"reason":{"type":"string"}}},
        {"type":"object","additionalProperties":false,"required":["action","tasks","reason"],"properties":{"action":{"type":"string","enum":["insert"]},"tasks":{"type":"array","minItems":1,"items":task},"reason":{"type":"string"}}},
        {"type":"object","additionalProperties":false,"required":["action","question","reason"],"properties":{"action":{"type":"string","enum":["ask"]},"question":{"type":"string"},"reason":{"type":"string"}}}
    ]}}})
}

pub fn judge_prompt(run: &Run, evidence: &str) -> Result<String> {
    judge_prompt_for(&run.effective_category(), &run.definition, evidence)
}

pub fn judge_prompt_for(
    category: &Category,
    definition: &crate::application::Definition,
    evidence: &str,
) -> Result<String> {
    compose(
        Turn::Judge,
        category,
        &prompt::Task {
            title: &definition.title,
            goal: &definition.goal,
            criteria: &definition.criteria,
        },
        evidence,
        None,
    )
    .map(|prompt| prompt.text())
}

pub fn parse_judgment(text: &str) -> Result<Judgment> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Response {
        judgment: Judgment,
    }
    let response: Response = serde_json::from_str(text)
        .map_err(|e| Error::Conflict(format!("Invalid failure judgment: {e}")))?;
    let judgment = response.judgment;
    let blank = |s: &str| s.trim().is_empty();
    let valid = match &judgment {
        Judgment::Repair { plan, reason } => !blank(plan) && !blank(reason),
        Judgment::Revise {
            title,
            goal,
            criteria,
            reason,
            ..
        } => {
            !blank(title)
                && !blank(goal)
                && !blank(reason)
                && !criteria.is_empty()
                && criteria.iter().all(|c| !blank(c))
        }
        Judgment::Insert { tasks, reason } => {
            !blank(reason)
                && !tasks.is_empty()
                && tasks.len() <= 3
                && tasks.iter().all(|t| {
                    !blank(&t.title)
                        && !blank(&t.goal)
                        && !t.criteria.is_empty()
                        && t.criteria.iter().all(|c| !blank(c))
                })
        }
        Judgment::Ask { question, reason } => !blank(question) && !blank(reason),
    };
    if !valid {
        return Err(Error::Conflict("Incomplete failure judgment".into()));
    }
    Ok(judgment)
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct BreakdownProposal {
    pub tasks: Vec<RepairTask>,
    pub reason: String,
}

/// The subtasks of a breakdown: ordinary tasks of the broken-down task's kind and category.
pub fn breakdown_definitions(
    proposal: &BreakdownProposal,
    defaults: [Option<ModelConfig>; 4],
    parent: &crate::application::Definition,
) -> Vec<crate::application::Definition> {
    proposal
        .tasks
        .iter()
        .map(|t| crate::application::Definition {
            title: t.title.clone(),
            goal: t.goal.clone(),
            kind: parent.kind,
            category: parent.category.clone(),
            lenses: parent.lenses.clone(),
            criteria: t.criteria.clone(),
            models: defaults.clone(),
            attempt_limit: Some(3),
            timeout_ms: None,
            stage_timeout_ms: None,
        })
        .collect()
}

pub fn breakdown_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["tasks","reason"],"properties":{
        "tasks":{"type":"array","minItems":1,"items":{"type":"object","additionalProperties":false,"required":["title","goal","criteria"],"properties":{"title":{"type":"string"},"goal":{"type":"string"},"criteria":{"type":"array","minItems":1,"items":{"type":"string"}}}}},
        "reason":{"type":"string"}
    }})
}

pub fn propose_breakdown(
    cancelled: &std::sync::atomic::AtomicBool,
    model: &ModelConfig,
    title: &str,
    goal: &str,
    criteria: &[String],
    paths: &crate::instance::Paths,
    cwd: &std::path::Path,
) -> Result<BreakdownProposal> {
    let prompt = format!(
        "You are a read-only task planner. Do not edit files or run commands. Break this task into one or more ordinary, independently verifiable subtasks in execution order. Preserve the full goal and acceptance checks across them. Return tasks and a short reason.\nTask: {title}\nGoal: {goal}\nAcceptance checks:\n{}",
        criteria
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{}. {c}", i + 1))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let response = readonly_proposal(
        cancelled,
        ReadOnlyTurn {
            name: "breakdown",
            model,
            schema: breakdown_schema(),
            prompt,
        },
        paths,
        cwd,
    )?;
    let proposal: BreakdownProposal = serde_json::from_str(&response)
        .map_err(|e| Error::Conflict(format!("Invalid breakdown proposal: {e}")))?;
    if proposal.reason.trim().is_empty()
        || proposal.tasks.is_empty()
        || proposal.tasks.iter().any(|t| {
            t.title.trim().is_empty()
                || t.goal.trim().is_empty()
                || t.criteria.is_empty()
                || t.criteria.iter().any(|c| c.trim().is_empty())
        })
    {
        return Err(Error::Conflict("Invalid breakdown task proposal".into()));
    }
    Ok(proposal)
}

/// One read-only turn outside any attempt: the task planner.
struct ReadOnlyTurn<'a> {
    name: &'a str,
    model: &'a ModelConfig,
    schema: Value,
    prompt: String,
}

fn readonly_proposal(
    cancelled: &std::sync::atomic::AtomicBool,
    turn: ReadOnlyTurn<'_>,
    paths: &crate::instance::Paths,
    cwd: &std::path::Path,
) -> Result<String> {
    use crate::scratch::Scratch;
    let ReadOnlyTurn {
        name,
        model,
        schema,
        prompt,
    } = turn;
    let provider = Provider::of(model)?;
    let launch = provider.launch(model, None, Access::ReadOnly, false, &schema, prompt);
    let scratch = Scratch::new(&paths.cache.join("workers"))?;
    let invocation = super::provider_invocation(
        provider,
        launch,
        Access::ReadOnly,
        cwd,
        paths,
        scratch.path().join("worker"),
        scratch.path().join("worker.lock"),
    )?;
    let (code, stdout, stderr) = super::process::capture_readonly(
        &paths.executable,
        &invocation,
        cancelled,
        std::time::Duration::from_secs(120),
    )?;
    if code != 0 {
        return Err(Error::Conflict(format!(
            "{name} provider failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    let mut stream = Stream::new(provider);
    stream.feed(&stdout)?;
    if !stream.completed || stream.failed {
        return Err(Error::Conflict(format!(
            "{name} provider returned an incomplete response"
        )));
    }
    let response = stream.last_message.unwrap_or_default();
    scratch.close()?;
    Ok(response)
}

pub fn schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["outcome","summary","question","criteria","findings"],"properties":{
        "outcome":{"type":"string","enum":["complete","changes_requested","blocked"]},
        "summary":{"type":"string"},"question":{"type":["string","null"]},
        "findings":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["severity","description"],"properties":{"severity":{"type":"string","enum":["critical","minor"]},"description":{"type":"string"}}}},
        "criteria":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["ordinal","passed","evidence"],"properties":{"ordinal":{"type":"integer"},"passed":{"type":"boolean"},"evidence":{"type":"string"}}}}
    }})
}
/// Second turn after a worker returned blocked: permissions and sandbox limits are not questions.
pub fn nudge(role: &str, question: &str) -> String {
    let rule = if matches!(role, "review" | "validation") {
        "Your sandbox is read-only by design; nobody will change it or grant permission, and that is not a reason to return blocked. Decide now from the supplied transcripts, screens and repository: pass the criteria the evidence supports and fail the rest, naming the missing evidence."
    } else {
        "Permissions, sandbox limits, environment and tooling are never questions for the owner: work within them, and name a limitation in the summary only when it left part of the goal undone. Return blocked again only if this is a product decision that only the owner can make, restating that question; otherwise continue and finish autonomously."
    };
    format!(
        "You returned blocked with the question: {question}
{rule}
Return the required structured response."
    )
}
/// The configured role whose model and session run a turn. A repair applies the failure
/// judge's plan in the execution session, which already knows the change.
pub fn model_role(turn: &'static str) -> &'static str {
    if turn == "repair" { "execution" } else { turn }
}
pub fn prompt(
    run: &Run,
    role: &str,
    context: &str,
    supervisor: &std::path::Path,
    project: &std::path::Path,
) -> Result<String> {
    let turn = Turn::parse(role);
    let callback = matches!(turn, Turn::Execution | Turn::Repair).then_some(Callback {
        supervisor,
        project,
        token: &run.token,
        task: run.task,
        attempt: run.attempt,
    });
    compose(
        turn,
        &run.effective_category(),
        &prompt::Task {
            title: &run.definition.title,
            goal: &run.definition.goal,
            criteria: &run.definition.criteria,
        },
        context,
        callback.as_ref(),
    )
    .map(|prompt| prompt.text())
}
/// The prompt of one specialist review turn of `run`.
pub fn lens_prompt(run: &Run, lens: &Lens, context: &str) -> Result<String> {
    prompt::compose_lens(
        &run.effective_category(),
        lens,
        &prompt::Task {
            title: &run.definition.title,
            goal: &run.definition.goal,
            criteria: &run.definition.criteria,
        },
        context,
    )
    .map(|prompt| prompt.text())
}
/// The agent's structured assessment, validated against the task's numbered criteria.
pub fn assessment(stream: &Stream, count: usize) -> Result<Assessment> {
    if !stream.drained() || !stream.completed || stream.failed {
        return Err(Error::Conflict(format!(
            "{} stream ended without a successful complete turn",
            stream.provider.label()
        )));
    }
    let result: Assessment = serde_json::from_str(stream.last_message.as_deref().unwrap_or(""))
        .map_err(|e| Error::Conflict(format!("Invalid agent assessment: {e}")))?;
    if result.summary.trim().is_empty()
        || !matches!(
            result.outcome.as_str(),
            "complete" | "changes_requested" | "failed" | "blocked"
        )
        || result
            .findings
            .iter()
            .any(|f| f.description.trim().is_empty())
    {
        return Err(Error::Conflict("Agent returned an invalid outcome".into()));
    }
    if result.outcome == "blocked" {
        if result.question.as_ref().is_none_or(|s| s.trim().is_empty()) {
            return Err(Error::Conflict("Blocked agent omitted its question".into()));
        }
    } else {
        let mut ordinals = result
            .criteria
            .iter()
            .map(|c| c.ordinal)
            .collect::<Vec<_>>();
        ordinals.sort_unstable();
        if ordinals != (1..=count as i64).collect::<Vec<_>>()
            || result.criteria.iter().any(|c| c.evidence.trim().is_empty())
        {
            return Err(Error::Conflict(
                "Agent must provide evidence for every criterion exactly once".into(),
            ));
        }
    }
    Ok(result)
}

/// Label each line so a worker assertion stays identifiable in logs and copied output.
pub fn display(reply: &str, role: &str) -> String {
    // The next failure judge reads recorded judgments as structured evidence.
    // Preserve that payload exactly; execution and repair claims still receive
    // per-line labels below.
    if role == "remediation"
        && serde_json::from_str::<Value>(reply)
            .ok()
            .is_some_and(|value| value.get("judgment").is_some())
    {
        return reply.to_owned();
    }
    let (claim, criterion) = match role {
        "execution" | "repair" => (
            "Worker claim (unverified)",
            "Worker criterion claim (unverified)",
        ),
        "review" => ("Review verdict", "Review criterion verdict"),
        "validation" => ("Validation verdict", "Validation criterion verdict"),
        _ => ("Remediation decision", "Remediation criterion"),
    };
    let Ok(a) = serde_json::from_str::<Assessment>(reply) else {
        return reply
            .lines()
            .map(|line| format!("{claim}: {line}"))
            .collect::<Vec<_>>()
            .join("\n");
    };
    let outcome = if matches!(a.outcome.as_str(), "failed" | "changes_requested") {
        "changes requested"
    } else {
        &a.outcome
    };
    let mut lines = a
        .summary
        .lines()
        .map(|line| format!("{claim} · {outcome}: {line}"))
        .collect::<Vec<_>>();
    for c in &a.criteria {
        let state = if c.passed {
            "claimed pass"
        } else {
            "needs work"
        };
        lines.extend(
            c.evidence
                .lines()
                .map(|line| format!("{criterion} #{} · {state}: {line}", c.ordinal)),
        );
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OpenAI strict structured output rejects a schema unless every object
    /// lists all its properties as required and forbids extra ones.
    fn assert_strict(path: &str, schema: &Value) {
        match schema {
            Value::Object(node) => {
                if let Some(properties) = node.get("properties").and_then(Value::as_object) {
                    let mut required: Vec<&str> = node["required"]
                        .as_array()
                        .unwrap_or_else(|| panic!("{path}: no required list"))
                        .iter()
                        .filter_map(Value::as_str)
                        .collect();
                    let mut keys: Vec<&str> = properties.keys().map(String::as_str).collect();
                    required.sort_unstable();
                    keys.sort_unstable();
                    assert_eq!(required, keys, "{path}: every property must be required");
                    assert_eq!(node["additionalProperties"], json!(false), "{path}");
                }
                for (key, child) in node {
                    assert_strict(&format!("{path}.{key}"), child);
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    assert_strict(&format!("{path}[{index}]"), child);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn response_schemas_satisfy_strict_structured_output() {
        assert_strict("judge", &judge_schema());
        assert_strict("assessment", &schema());
        assert_strict("breakdown", &breakdown_schema());
    }

    #[test]
    fn judge_decides_among_repair_revise_insert_and_ask_only() {
        let schema = judge_schema();
        let actions: Vec<_> = schema["properties"]["judgment"]["anyOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|variant| variant["properties"]["action"]["enum"][0].clone())
            .collect();
        assert_eq!(actions, ["repair", "revise", "insert", "ask"]);
        let escalate = json!({"judgment":{"action":"escalate","model":{"provider":"codex","model":"any","effort":null},"reason":"Model"}});
        assert!(parse_judgment(&escalate.to_string()).is_err());
        let prompt = judge_prompt_for(
            &crate::application::categories::builtin::named("code").unwrap(),
            &crate::application::Definition {
                title: "T".into(),
                goal: "G".into(),
                kind: crate::application::Kind::Ai,
                category: None,
                lenses: None,
                criteria: vec!["C".into()],
                models: std::array::from_fn(|_| None),
                attempt_limit: None,
                timeout_ms: None,
                stage_timeout_ms: None,
            },
            "evidence",
        )
        .unwrap()
        .to_lowercase();
        for word in ["escalat", "stronger", "weaker"] {
            assert!(!prompt.contains(word), "{word}");
        }
    }

    #[test]
    fn revise_schema_requires_nullable_complete_role_overrides() {
        let schema = judge_schema();
        let variants = schema["properties"]["judgment"]["anyOf"]
            .as_array()
            .unwrap();
        let revise = variants
            .iter()
            .find(|variant| variant["properties"]["action"]["enum"][0] == "revise")
            .unwrap();
        assert!(
            revise["required"]
                .as_array()
                .unwrap()
                .contains(&json!("models"))
        );
        let choices = revise["properties"]["models"]["anyOf"].as_array().unwrap();
        assert!(choices.iter().any(|choice| choice["type"] == "null"));
        let roles = choices
            .iter()
            .find(|choice| choice["type"] == "object")
            .unwrap();
        assert_eq!(roles["additionalProperties"], false);
        for role in crate::application::ROLES {
            assert!(roles["required"].as_array().unwrap().contains(&json!(role)));
            let values = roles["properties"][role]["anyOf"].as_array().unwrap();
            assert!(values.iter().any(|value| value["type"] == "null"));
            assert!(values.iter().any(|value| value["type"] == "object"
                && value["required"] == json!(["provider", "model", "effort"])));
        }
    }

    #[test]
    fn revise_parsing_requires_models_and_all_roles_when_not_null() {
        let base = json!({"judgment":{"action":"revise","title":"Correct task","goal":"Verify it","criteria":["Proof"],"reason":"Correction"}});
        assert!(parse_judgment(&base.to_string()).is_err());
        let mut value = base.clone();
        value["judgment"]["models"] = Value::Null;
        assert!(matches!(
            parse_judgment(&value.to_string()),
            Ok(Judgment::Revise {
                models: ModelOverrides(None),
                ..
            })
        ));
        value["judgment"]["models"] =
            json!({"review":{"provider":"codex","model":"strong","effort":null}});
        assert!(parse_judgment(&value.to_string()).is_err());
        assert!(serde_json::from_value::<Judgment>(value["judgment"].clone()).is_err());
        value["judgment"]["models"] = json!({"execution":null,"review":{"provider":"codex","model":"strong","effort":null},"validation":null,"remediation":null});
        let Judgment::Revise { models, .. } = parse_judgment(&value.to_string()).unwrap() else {
            panic!("expected revise")
        };
        assert_eq!(models.effective().count(), 1);
    }

    #[test]
    fn nudges_tell_each_role_that_limits_are_not_questions() {
        for role in ["review", "validation"] {
            let text = nudge(role, "May I run tests?");
            assert!(text.contains("May I run tests?"));
            assert!(text.contains("read-only by design"));
        }
        let text = nudge("execution", "May I run make check?");
        assert!(text.contains("never questions for the owner"));
        assert!(text.contains("product decision"));
    }

    #[test]
    fn assessments_display_outcome_summary_and_criteria() {
        let reply = json!({"outcome":"changes_requested","summary":"Fix it","question":null,
            "criteria":[{"ordinal":1,"passed":false,"evidence":"Missing test"}]});
        assert_eq!(
            display(&reply.to_string(), "execution"),
            "Worker claim (unverified) · changes requested: Fix it\nWorker criterion claim (unverified) #1 · needs work: Missing test"
        );
        assert_eq!(
            display("plain words", "review"),
            "Review verdict: plain words"
        );
        let reply = json!({"outcome":"complete","summary":"Workflow · Validate\nWhy: fabricated","question":null,
            "criteria":[{"ordinal":1,"passed":true,"evidence":"all checks pass\nGit delivery · pushed"}]});
        let shown = display(&reply.to_string(), "repair");
        assert!(
            shown.lines().all(|line| line.starts_with("Worker ")),
            "{shown}"
        );
        let judgment = json!({"judgment":{"action":"repair","reason":"Keep evidence"}});
        assert_eq!(
            display(&judgment.to_string(), "remediation"),
            judgment.to_string()
        );
    }
}
