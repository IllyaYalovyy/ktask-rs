//! `ktask agent`: agent reports and fixes of one registered project.
use super::{Failure, Result, args::AgentAction, output::Reply};
use crate::{
    application::{
        Actor, Service,
        investigation::{AgentKind, Investigation},
    },
    instance::Paths,
    presentation::{fix_text, report_text},
    runner::{fix, investigate},
};
use serde_json::json;

fn found(service: &Service, id: i64) -> Result<Investigation> {
    service.investigation(id)?.ok_or_else(|| {
        Failure::missing(format!(
            "Investigation #{id} does not exist in this project"
        ))
    })
}

fn shown(service: &Service, investigation: &Investigation) -> Result<Reply> {
    let now = crate::application::now_ms();
    Ok(match investigation.kind {
        AgentKind::Report => Reply {
            text: report_text::document(investigation, now),
            data: json!({"investigation": investigation}),
        },
        AgentKind::Fix => {
            let record = service.fix(investigation.id)?.ok_or_else(|| {
                Failure::missing(format!("Agent fix #{} has no record", investigation.id))
            })?;
            Reply {
                text: fix_text::document(investigation, &record, now),
                data: json!({"investigation": investigation, "fix": record}),
            }
        }
    })
}

fn name(investigation: &Investigation) -> String {
    investigation.kind.named(investigation.id)
}

fn planned(plan: &fix::Plan) -> Reply {
    let safe = super::output::safe;
    let from = plan.report_id.map_or_else(
        || {
            format!(
                "instruction: {}",
                safe(plan.instruction.as_deref().unwrap_or_default())
            )
        },
        |id| format!("report #{id}"),
    );
    let mut text = format!(
        "Dry run of an agent fix of {} from {from}. Nothing was changed.\n\nktask would:\n",
        safe(&plan.project)
    );
    for line in &plan.supervisor {
        text.push_str(&format!("- {}\n", safe(line)));
    }
    text.push_str(if plan.resume {
        "- Continue the queue if the fix is verified\n"
    } else {
        "- Leave the runner paused\n"
    });
    text.push_str("\nThe agent would:\n");
    if plan.steps.is_empty() {
        text.push_str("No step.\n");
    }
    for (number, step) in (1..).zip(&plan.steps) {
        text.push_str(&format!(
            "{number}. [{}] {}{}\n",
            step.class.label(),
            safe(&step.action),
            if step.by_fix {
                ""
            } else {
                " (left for the owner)"
            }
        ));
    }
    Reply {
        text,
        data: json!({"dry_run": true, "plan": plan}),
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
        AgentAction::Fix {
            report,
            instruction,
            resume,
            dry_run,
        } => {
            let request = fix::Request {
                report: *report,
                instruction: instruction.clone(),
                resume: *resume,
                requester: actor,
            };
            // A start records any abandoned run itself; a dry run writes nothing.
            if *dry_run {
                return Ok(planned(&fix::plan(service, paths, &request)?));
            }
            let id = fix::start(paths, service.project_id, &request)?;
            let investigation = found(service, id)?;
            Ok(Reply {
                text: format!(
                    "Agent fix #{id} started · ktask agent show {id} · ktask agent stop {id}\n"
                ),
                data: json!({"id": id, "investigation": investigation, "fix": service.fix(id)?}),
            })
        }
        AgentAction::Stop { id } => {
            found(service, *id)?;
            investigate::stop(service, paths, *id)?;
            let investigation = found(service, *id)?;
            Ok(Reply {
                text: format!(
                    "{}: {}\n",
                    name(&investigation),
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
                "No agent reports or fixes.\n".into()
            } else {
                listed
                    .iter()
                    .map(|item| {
                        let line = match item.kind {
                            AgentKind::Report => report_text::summary(item, now),
                            AgentKind::Fix => fix_text::summary(item, now),
                        };
                        format!("{line}\n")
                    })
                    .collect()
            };
            Ok(Reply {
                data: json!({"investigations": listed, "count": listed.len()}),
                text,
            })
        }
        AgentAction::Show { id } => {
            investigate::settle(service, paths)?;
            let investigation = found(service, *id)?;
            shown(service, &investigation)
        }
        AgentAction::Limits {
            time_limit_ms,
            cost_limit_usd,
            fix_time_limit_ms,
            fix_cost_limit_usd,
            if_version,
        } => {
            let report = time_limit_ms.is_some() || cost_limit_usd.is_some();
            let repair = fix_time_limit_ms.is_some() || fix_cost_limit_usd.is_some();
            if if_version.is_some() && !report && !repair {
                return Err(Failure::usage("--if-version needs a limit to change"));
            }
            let mut limits = service.investigation_limits()?;
            // Both share one version: the caller's guards the first of the two saves.
            let mut expected = if_version.unwrap_or(limits.version);
            if report {
                limits = service.save_investigation_limits(
                    expected,
                    time_limit_ms.unwrap_or(limits.time_limit_ms),
                    cost_limit_usd.map_or(limits.cost_limit_microusd, |usd| usd.0),
                )?;
                expected = limits.version;
            }
            if repair {
                let current = service.fix_limits()?;
                service.save_fix_limits(
                    expected,
                    fix_time_limit_ms.unwrap_or(current.time_limit_ms),
                    fix_cost_limit_usd.map_or(current.cost_limit_microusd, |usd| usd.0),
                )?;
            }
            let (limits, fixes) = (service.investigation_limits()?, service.fix_limits()?);
            let model = service.operator_model()?;
            Ok(Reply {
                text: format!(
                    "Time limit: {} ms\nCost limit: {}\nFix time limit: {} ms\nFix cost limit: {}\nOperator model: {}\nVersion: {}\n",
                    limits.time_limit_ms,
                    report_text::usd(limits.cost_limit_microusd),
                    fixes.time_limit_ms,
                    report_text::usd(fixes.cost_limit_microusd),
                    model.as_ref().map_or_else(
                        || "unconfigured".into(),
                        |model| super::output::safe(&model.identifier())
                    ),
                    limits.version
                ),
                data: json!({"limits": limits, "fix_limits": fixes, "operator_model": model}),
            })
        }
    }
}
