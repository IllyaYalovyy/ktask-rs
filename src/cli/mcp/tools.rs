use super::super::{Failure, Result, args, execute, output, projects};
use crate::{
    application::{Actor, Selector, Service, task_query},
    instance::Paths,
    storage::Database,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

const DEFAULT_LIMIT: i64 = 25;
const MAX_LIMIT: i64 = 100;
const EVIDENCE_CHARS: usize = 4000;

/// What a server session knows about the host. Projects are opened per call, so one that was
/// deleted, moved or created meanwhile is seen as it is then.
pub(crate) struct Host {
    pub paths: Paths,
    pub version: &'static str,
    /// The only project this server may act on, when started with `--project`.
    pub pin: Option<i64>,
    /// The server's working directory, which selects a project when none is named.
    pub directory: Option<PathBuf>,
    pub actor: Actor,
}

/// A project named by a tool call: its ID, or the absolute folder it is registered at.
#[derive(Deserialize)]
#[serde(untagged)]
enum ProjectRef {
    Id(i64),
    Text(String),
}

const USE_PROJECTS: &str = "use the projects tool to list project IDs and folders";

impl Host {
    /// The project a call acts on, opened fresh: the one it names, else the pinned project,
    /// else the registered project containing the working directory.
    fn open(&self, named: Option<ProjectRef>) -> Result<Service> {
        let folder;
        let selector = match (named, self.pin, &self.directory) {
            (Some(ProjectRef::Id(id)), ..) => Selector::Id(id),
            (Some(ProjectRef::Text(text)), ..) => match text.trim().parse::<i64>() {
                Ok(id) => Selector::Id(id),
                Err(_) => {
                    folder = PathBuf::from(&text);
                    if !folder.is_absolute() {
                        return Err(Failure::usage(format!(
                            "project must be a project ID or an absolute folder; {USE_PROJECTS}"
                        )));
                    }
                    Selector::Folder(&folder)
                }
            },
            (None, Some(id), _) => Selector::Id(id),
            (None, None, Some(directory)) => Selector::Containing(directory),
            (None, None, None) => {
                return Err(self.no_project("The server has no working directory"));
            }
        };
        let unregistered = |what: String| {
            Failure::missing(format!(
                "{what} is not a registered project; {USE_PROJECTS}"
            ))
        };
        let db = Database::open_existing(&self.paths)?
            .ok_or_else(|| Failure::missing(format!("No project is registered; {USE_PROJECTS}")))?;
        let mut service = Service::select(db, selector)?.ok_or_else(|| match selector {
            Selector::Id(id) => unregistered(format!("Project {id}")),
            Selector::Folder(folder) => unregistered(folder.display().to_string()),
            Selector::Containing(_) => {
                self.no_project("The server's working directory is not inside a registered project")
            }
        })?;
        if self.pin.is_some_and(|pin| pin != service.project_id) {
            return Err(Failure::usage(format!(
                "This server is pinned to one project and refuses {} (project {}); {USE_PROJECTS}",
                service.project_name, service.project_id
            )));
        }
        service.set_actor(self.actor.clone())?;
        Ok(service)
    }

    fn no_project(&self, why: &str) -> Failure {
        Failure::missing(format!(
            "{why}, so name a project with the project argument; {USE_PROJECTS}"
        ))
    }
}

/// The project a result is about, as every tool names it.
fn named(service: &Service, mut data: Value) -> Value {
    data["project"] = json!({"id":service.project_id,"name":service.project_name});
    data
}

#[derive(Clone, Copy)]
pub(super) enum Tool {
    Projects,
    Current,
    List,
    Show,
    Events,
    AddDraft,
}

impl Tool {
    pub(super) const ALL: [Self; 6] = [
        Self::Projects,
        Self::Current,
        Self::List,
        Self::Show,
        Self::Events,
        Self::AddDraft,
    ];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Projects => "projects",
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
        let (description, mut properties, required) = match self {
            Self::Projects => (
                "List every project of this ktask instance with its state, queue size and current task.",
                json!({}),
                json!([]),
            ),
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
        if !matches!(self, Self::Projects) {
            properties["project"] = json!({
                "anyOf":[{"type":"integer","minimum":1},{"type":"string","minLength":1}],
                "description":"Project ID or absolute folder from the projects tool. Defaults to the pinned project, else the registered project containing the server's working directory.",
            });
        }
        let read_only = !matches!(self, Self::AddDraft);
        json!({
            "name": self.name(),
            "description": description,
            "inputSchema": {"type":"object","properties":properties,"required":required,"additionalProperties":false},
            "annotations": {"readOnlyHint":read_only,"destructiveHint":false,"idempotentHint":read_only,"openWorldHint":false},
        })
    }

    pub(super) fn invoke(self, host: &Host, arguments: &Value) -> Result<Value> {
        if matches!(self, Self::Projects) {
            read_arguments::<Empty>(arguments)?;
            return Ok(projects(&host.paths)?.data);
        }
        let mut arguments = arguments.clone();
        let project = arguments
            .as_object_mut()
            .and_then(|arguments| arguments.remove("project"))
            .map(ProjectRef::deserialize)
            .transpose()
            .map_err(|_| Failure::usage("project must be a project ID or an absolute folder"))?;
        let arguments = &arguments;
        let mut service = host.open(project)?;
        let service = &mut service;
        let data = match self {
            Self::Projects => unreachable!("handled above"),
            Self::Current => {
                read_arguments::<Empty>(arguments)?;
                json!({"task":service.task_detail(None)?.map(task)})
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
                json!({"tasks":rows,"count":rows.len(),"offset":offset,"next_offset":next_offset})
            }
            Self::Show => {
                let Show { id } = read_arguments(arguments)?;
                if id <= 0 {
                    return Err(Failure::usage("id must be positive"));
                }
                let detail = service.task_detail(Some(id))?.ok_or_else(|| {
                    Failure::missing(format!("Task #{id} does not exist in this project"))
                })?;
                json!({"task":task(detail)})
            }
            Self::Events => {
                let Events { after, limit } = read_arguments(arguments)?;
                validate_page(after, limit)?;
                let mut events = service.events_after(after, limit)?;
                let next_after = events.last().map_or(after, |event| event.id);
                for event in &mut events {
                    excerpt(&mut event.message);
                }
                json!({"events":events,"next_after":next_after})
            }
            Self::AddDraft => {
                let Draft {
                    title,
                    goal,
                    criteria,
                    kind,
                } = read_arguments(arguments)?;
                execute::add(
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
                .data
            }
        };
        Ok(named(service, data))
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
