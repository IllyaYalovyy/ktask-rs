//! `ktask agent`: agent reports of one registered project.
use super::{Failure, Result, args::AgentAction, output::Reply};
use crate::{
    application::{Actor, Service, investigation::Investigation},
    instance::Paths,
    presentation::report_text,
    runner::investigate,
};
use serde_json::json;

fn found(service: &Service, id: i64) -> Result<Investigation> {
    service.investigation(id)?.ok_or_else(|| {
        Failure::missing(format!(
            "Investigation #{id} does not exist in this project"
        ))
    })
}

fn shown(investigation: &Investigation) -> Reply {
    Reply {
        text: report_text::document(investigation, crate::application::now_ms()),
        data: json!({"investigation": investigation}),
    }
}

pub(super) fn run(
    service: &mut Service,
    paths: &Paths,
    actor: Actor,
    action: &AgentAction,
) -> Result<Reply> {
    match action {
        AgentAction::Report { task, question } => {
            if let Some(task) = task {
                service.task(*task)?.ok_or_else(|| {
                    Failure::missing(format!("Task #{task} does not exist in this project"))
                })?;
            }
            let id = investigate::start(
                paths,
                service.project_id,
                &investigate::Request {
                    task: *task,
                    question: question.clone(),
                    requester: actor,
                },
            )?;
            let investigation = found(service, id)?;
            Ok(Reply {
                text: format!(
                    "Agent report #{id} started · ktask agent show {id} · ktask agent stop {id}\n"
                ),
                data: json!({"id": id, "investigation": investigation}),
            })
        }
        AgentAction::Stop { id } => {
            found(service, *id)?;
            investigate::stop(service, paths, *id)?;
            let investigation = found(service, *id)?;
            Ok(Reply {
                text: format!(
                    "Agent report #{id}: {}\n",
                    investigation.state.label().to_lowercase()
                ),
                data: json!({"id": id, "investigation": investigation}),
            })
        }
        AgentAction::List => {
            investigate::settle(service, paths)?;
            let listed = service.investigations()?;
            let now = crate::application::now_ms();
            let text = if listed.is_empty() {
                "No agent reports.\n".into()
            } else {
                listed
                    .iter()
                    .map(|item| format!("{}\n", report_text::summary(item, now)))
                    .collect()
            };
            Ok(Reply {
                data: json!({"investigations": listed, "count": listed.len()}),
                text,
            })
        }
        AgentAction::Show { id } => {
            investigate::settle(service, paths)?;
            Ok(shown(&found(service, *id)?))
        }
        AgentAction::Limits {
            time_limit_ms,
            cost_limit_usd,
            if_version,
        } => {
            let mut limits = service.investigation_limits()?;
            if time_limit_ms.is_some() || cost_limit_usd.is_some() {
                limits = service.save_investigation_limits(
                    if_version.unwrap_or(limits.version),
                    time_limit_ms.unwrap_or(limits.time_limit_ms),
                    cost_limit_usd.map_or(limits.cost_limit_microusd, |usd| usd.0),
                )?;
            } else if if_version.is_some() {
                return Err(Failure::usage("--if-version needs a limit to change"));
            }
            let model = service.operator_model()?;
            Ok(Reply {
                text: format!(
                    "Time limit: {} ms\nCost limit: {}\nOperator model: {}\nVersion: {}\n",
                    limits.time_limit_ms,
                    report_text::usd(limits.cost_limit_microusd),
                    model.as_ref().map_or_else(
                        || "unconfigured".into(),
                        |model| super::output::safe(&model.identifier())
                    ),
                    limits.version
                ),
                data: json!({"limits": limits, "operator_model": model}),
            })
        }
    }
}
