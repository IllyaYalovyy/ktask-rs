use super::super::{Failure, Result, args, execute, output};
use crate::application::{Service, task_query};
use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_LIMIT: i64 = 25;
const MAX_LIMIT: i64 = 100;
const EVIDENCE_CHARS: usize = 4000;

#[derive(Clone, Copy)]
pub(super) enum Tool {
    Current,
    List,
    Show,
    Events,
    AddDraft,
}

impl Tool {
    pub(super) const ALL: [Self; 5] = [
        Self::Current,
        Self::List,
        Self::Show,
        Self::Events,
        Self::AddDraft,
    ];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::List => "list",
            Self::Show => "show",
            Self::Events => "events",
            Self::AddDraft => "add_draft",
        }
    }

    pub(super) fn spec(self) -> Value {
        let limit =
            json!({"type":"integer","minimum":1,"maximum":MAX_LIMIT,"default":DEFAULT_LIMIT});
        let cursor = json!({"type":"integer","minimum":0,"maximum":i64::MAX,"default":0});
        let text = json!({"type":"string","minLength":1,"pattern":"\\S"});
        let (description, properties, required) = match self {
            Self::Current => (
                "Read the current task and what it is waiting on, or null if the queue is empty.",
                json!({}),
                json!([]),
            ),
            Self::List => (
                "Page through task summaries in queue order or newest resolution first in history.",
                json!({"scope":{"type":"string","enum":["queue","history","all"],"default":"queue"},"offset":cursor,"limit":limit}),
                json!([]),
            ),
            Self::Show => (
                "Read one task's definition, latest attempt and dependencies.",
                json!({"id":{"type":"integer","minimum":1,"maximum":i64::MAX}}),
                json!(["id"]),
            ),
            Self::Events => (
                "Read project events with IDs greater than after, in ascending order.",
                json!({"after":cursor,"limit":limit}),
                json!([]),
            ),
            Self::AddDraft => (
                "Append an AI-attributed draft task for approval.",
                json!({"title":text,"goal":text,"criteria":{"type":"array","items":text,"minItems":1},"kind":{"type":"string","enum":["ai","human","docs"],"default":"ai"}}),
                json!(["title", "goal", "criteria"]),
            ),
        };
        let read_only = !matches!(self, Self::AddDraft);
        json!({
            "name": self.name(),
            "description": description,
            "inputSchema": {"type":"object","properties":properties,"required":required,"additionalProperties":false},
            "annotations": {"readOnlyHint":read_only,"destructiveHint":false,"idempotentHint":read_only,"openWorldHint":false},
        })
    }

    pub(super) fn invoke(self, service: &mut Service, arguments: &Value) -> Result<Value> {
        match self {
            Self::Current => {
                read_arguments::<Empty>(arguments)?;
                Ok(json!({"task":service.task_detail(None)?.map(task)}))
            }
            Self::List => {
                let List {
                    scope,
                    offset,
                    limit,
                } = read_arguments(arguments)?;
                validate_page(offset, limit)?;
                let mut rows = service.task_summaries(scope, offset, limit + 1)?;
                let next_offset = (rows.len() > limit as usize).then(|| {
                    rows.pop();
                    offset.saturating_add(limit)
                });
                Ok(
                    json!({"tasks":rows,"count":rows.len(),"offset":offset,"next_offset":next_offset}),
                )
            }
            Self::Show => {
                let Show { id } = read_arguments(arguments)?;
                if id <= 0 {
                    return Err(Failure::usage("id must be positive"));
                }
                let detail = service.task_detail(Some(id))?.ok_or_else(|| {
                    Failure::missing(format!("Task #{id} does not exist in this project"))
                })?;
                Ok(json!({"task":task(detail)}))
            }
            Self::Events => {
                let Events { after, limit } = read_arguments(arguments)?;
                validate_page(after, limit)?;
                let mut events = service.events_after(after, limit)?;
                let next_after = events.last().map_or(after, |event| event.id);
                for event in &mut events {
                    excerpt(&mut event.message);
                }
                Ok(json!({"events":events,"next_after":next_after}))
            }
            Self::AddDraft => {
                let Draft {
                    title,
                    goal,
                    criteria,
                    kind,
                } = read_arguments(arguments)?;
                Ok(execute::add(
                    service,
                    &args::Add {
                        title,
                        goal,
                        criterion: criteria,
                        kind,
                        category: None,
                        lenses: Default::default(),
                        approval: args::Approval::Draft,
                        before: None,
                        settings: Default::default(),
                    },
                )?
                .data)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Show {
    id: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    #[serde(default)]
    scope: task_query::Scope,
    #[serde(default)]
    offset: i64,
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Events {
    #[serde(default)]
    after: i64,
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Draft {
    title: String,
    goal: String,
    criteria: Vec<String>,
    #[serde(default)]
    kind: args::Kind,
}

fn default_limit() -> i64 {
    DEFAULT_LIMIT
}

fn read_arguments<'a, T: Deserialize<'a>>(arguments: &'a Value) -> Result<T> {
    T::deserialize(arguments).map_err(|error| Failure::usage(error.to_string()))
}

fn validate_page(start: i64, limit: i64) -> Result<()> {
    if start < 0 || !(1..=MAX_LIMIT).contains(&limit) {
        return Err(Failure::usage(format!(
            "Offset and event cursor must be nonnegative and limit must be 1–{MAX_LIMIT}"
        )));
    }
    Ok(())
}

fn excerpt(text: &mut String) {
    if let Some((byte, _)) = text.char_indices().nth(EVIDENCE_CHARS) {
        text.truncate(byte);
        text.push('…');
    }
}

fn task(mut detail: task_query::Detail) -> Value {
    for text in [&mut detail.launch_issue, &mut detail.resolution_reason]
        .into_iter()
        .filter_map(Option::as_mut)
    {
        excerpt(text);
    }
    if let Some(attempt) = &mut detail.latest_attempt {
        for text in [&mut attempt.report, &mut attempt.question]
            .into_iter()
            .filter_map(Option::as_mut)
        {
            excerpt(text);
        }
    }
    let mut data = json!(detail.summary);
    data["definition"] = output::definition(&detail.selected.definition);
    data["revision_actor"] = json!(detail.selected.actor);
    data["pending_draft"] = json!(detail.pending_draft.map(|draft| json!({
        "number":draft.number,"actor":draft.actor,"definition":output::definition(&draft.definition)
    })));
    data["latest_attempt"] = json!(detail.latest_attempt);
    data["dependencies"] = json!(detail.dependencies);
    data["launch_issue"] = json!(detail.launch_issue);
    data["resolution_reason"] = json!(detail.resolution_reason);
    data
}
