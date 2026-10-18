use super::{
    Failure, Result,
    args::*,
    output::{self, Reply},
};
use crate::application::{
    self as app, Definition, ModelConfig, Service, Task, VersionedTask, transfer,
};
use serde_json::json;

impl Settings {
    pub(super) fn empty(&self) -> bool {
        self.model.is_empty()
            && self.effort.is_empty()
            && self.clear_model.is_empty()
            && self.attempt_limit.is_none()
            && self.timeout_ms.is_none()
    }
    pub(super) fn validate(&self) -> Result<()> {
        for role in 0..4 {
            if self.model.iter().filter(|m| m.role == role).count()
                + self.clear_model.iter().filter(|r| **r == role).count()
                > 1
                || self.effort.iter().filter(|m| m.role == role).count() > 1
                || (self.clear_model.contains(&role) && self.effort.iter().any(|m| m.role == role))
            {
                return Err(Failure::usage(format!(
                    "Duplicate/conflicting settings for {}",
                    app::ROLES[role]
                )));
            }
        }
        Ok(())
    }
    fn apply(&self, d: &mut Definition) -> Result<()> {
        for &role in &self.clear_model {
            d.models[role] = None;
        }
        for setting in &self.model {
            let effort = d.models[setting.role]
                .as_ref()
                .and_then(|m| m.effort.clone());
            d.models[setting.role] = Some(ModelConfig {
                provider: setting.provider.clone(),
                model: setting.model.clone(),
                effort,
            });
        }
        for setting in &self.effort {
            let model = d.models[setting.role].as_mut().ok_or_else(|| {
                Failure::usage(format!(
                    "Configure a {} model before setting its effort",
                    app::ROLES[setting.role]
                ))
            })?;
            model.effort.clone_from(&setting.effort);
        }
        if let Some(limit) = self.attempt_limit {
            d.attempt_limit = limit.0;
        }
        if let Some(limit) = self.timeout_ms {
            d.timeout_ms = limit.0;
        }
        d.validate()?;
        Ok(())
    }
}

fn kind(kind: Kind) -> app::Kind {
    match kind {
        Kind::Ai => app::Kind::Ai,
        Kind::Human => app::Kind::Human,
    }
}
fn task(service: &mut Service, id: i64) -> Result<Task> {
    service
        .task(id)?
        .ok_or_else(|| Failure::missing(format!("Task #{id} does not exist in this project")))
}
fn target(service: &mut Service, input: &Target) -> Result<(Task, VersionedTask)> {
    let task = task(service, input.id)?;
    let version = input.if_version.unwrap_or(task.version);
    if version != task.version {
        return Err(crate::storage::Error::Conflict(format!(
            "Task #{} is version {}, not {}; read it again before retrying",
            input.id, task.version, version
        ))
        .into());
    }
    Ok((
        task,
        VersionedTask {
            id: input.id,
            version,
        },
    ))
}
fn receipt(action: &str, target: VersionedTask) -> Reply {
    Reply {
        data: json!({"action":action,"id":target.id,"version":target.version + 1}),
        text: format!("{action}: #{} v{}\n", target.id, target.version + 1),
    }
}
fn created(ids: Vec<i64>) -> Reply {
    Reply {
        text: ids
            .iter()
            .map(|id| format!("created: #{id} v1\n"))
            .collect(),
        data: json!({"action":"created","tasks":ids.iter().map(|id| json!({"id":id,"version":1})).collect::<Vec<_>>()}),
    }
}
fn tasks(service: &mut Service, scope: Scope) -> Result<Vec<Task>> {
    if matches!(scope, Scope::Deleted) {
        return Ok(service.deleted()?);
    }
    let snapshot = service.snapshot()?;
    Ok(match scope {
        Scope::Queue => snapshot.queue,
        Scope::History => snapshot.history,
        Scope::All => snapshot.queue.into_iter().chain(snapshot.history).collect(),
        Scope::Deleted => unreachable!(),
    })
}

pub(super) fn command(service: &mut Service, command: &Command) -> Result<Reply> {
    match command {
        Command::Run { .. } | Command::Recover { .. } => unreachable!("runner dispatch"),
        Command::Pause { target: input } => {
            let (_, target) = target(service, input)?;
            service.request_pause(target)?;
            Ok(Reply {
                data: json!({"action":"pause_requested","id":target.id}),
                text: "Pause requested; worker shutdown pending\n".into(),
            })
        }
        Command::Abandon {
            target: input,
            reason,
        } => {
            let (_, target) = target(service, input)?;
            crate::runner::abandon(service, target, reason)?;
            Ok(Reply {
                data: json!({"action":"abandoned","id":target.id}),
                text: "Attempt abandoned; resolve the task or run it again\n".into(),
            })
        }
        Command::Answer {
            target: input,
            text,
        } => {
            let (_, target) = target(service, input)?;
            service.answer(target, text)?;
            Ok(receipt("answered", target))
        }
        Command::CheckSet {
            name,
            command,
            timeout_ms,
        } => {
            service.set_check(name, command, *timeout_ms)?;
            Ok(Reply {
                data: json!({"checks":service.checks()?}),
                text: format!("Check saved: {name}\n"),
            })
        }
        Command::Checks => {
            let checks = service.checks()?;
            Ok(Reply {
                data: json!({"checks":checks}),
                text: checks
                    .iter()
                    .map(|c| {
                        format!(
                            "{}\t{}\t{} ms\n",
                            output::safe(&c.name),
                            output::safe(&c.command),
                            c.timeout_ms
                        )
                    })
                    .collect(),
            })
        }
        Command::ModelDefault {
            role,
            provider,
            model,
            effort,
        } => {
            service.set_default(
                role,
                &ModelConfig {
                    provider: provider.clone(),
                    model: model.clone(),
                    effort: effort.clone(),
                },
            )?;
            Ok(Reply {
                data: json!({"role":role,"provider":provider,"model":model,"effort":effort}),
                text: format!("Default saved for {role}; existing tasks unchanged\n"),
            })
        }
        Command::List {
            scope,
            search,
            kind: filter,
        } => {
            let mut rows = tasks(service, *scope)?;
            rows.retain(|t| {
                filter.is_none_or(|k| t.definition().definition.kind == kind(k))
                    && search.as_ref().is_none_or(|s| {
                        format!("#{} {} {}", t.id, t.status, t.definition().definition.title)
                            .to_lowercase()
                            .contains(&s.to_lowercase())
                    })
            });
            Ok(Reply::list(&rows))
        }
        Command::Show { id } => Ok(Reply::task(&task(service, *id)?)),
        Command::Current => Ok(service.snapshot()?.queue.first().map_or(
            Reply {
                data: json!(null),
                text: "No current task.\n".into(),
            },
            Reply::task,
        )),
        Command::Add(add) => {
            let mut d = Definition {
                title: add.title.clone(),
                goal: add.goal.clone(),
                kind: kind(add.kind),
                criteria: add.criterion.clone(),
                models: service.defaults()?,
                attempt_limit: Some(3),
                timeout_ms: Some(600_000),
            };
            add.settings.apply(&mut d)?;
            Ok(created(service.create(
                &[d],
                add.before,
                matches!(add.approval, Approval::Ready),
            )?))
        }
        Command::Edit {
            target: input,
            pending,
            changes,
        } => edit(service, input, *pending, changes),
        Command::Move {
            target: input,
            before,
        } => {
            let (_, target) = target(service, input)?;
            service.move_before(target, *before)?;
            Ok(receipt("moved", target))
        }
        Command::Approve { target: input } => {
            let (_, target) = target(service, input)?;
            service.decide(target, true, "Approved via CLI")?;
            Ok(receipt("approved", target))
        }
        Command::Reject {
            target: input,
            reason,
        } => {
            let (_, target) = target(service, input)?;
            service.decide(target, false, reason)?;
            Ok(receipt("rejected", target))
        }
        Command::Resolve {
            target: input,
            outcome,
            reason,
        } => {
            let (_, target) = target(service, input)?;
            service.resolve(
                target,
                match outcome {
                    Outcome::SucceededOverride => app::Resolution::SucceededOverride,
                    Outcome::FailedSkipped => app::Resolution::FailedSkipped,
                    Outcome::Superseded => app::Resolution::Superseded,
                },
                reason,
            )?;
            Ok(receipt("resolved", target))
        }
        Command::Ack { target: input } => {
            let (_, target) = target(service, input)?;
            service.acknowledge(target)?;
            Ok(receipt("acknowledged", target))
        }
        Command::Delete { target: input } => {
            let (_, target) = target(service, input)?;
            service.delete(target)?;
            Ok(receipt("deleted", target))
        }
        Command::Restore { target: input } => {
            let (_, target) = target(service, input)?;
            service.restore(target)?;
            Ok(receipt("restored", target))
        }
        Command::Import { file, before, .. } => {
            Ok(created(service.import(&transfer::import(file)?, *before)?))
        }
        Command::Export {
            file,
            scope,
            overwrite,
        } => {
            let definitions = tasks(service, *scope)?
                .into_iter()
                .map(|t| t.definition().definition.clone())
                .collect::<Vec<_>>();
            let data = transfer::encode(&definitions)?;
            let expected = if file.try_exists()? {
                if !overwrite {
                    return Err(crate::storage::Error::Conflict(
                        "Destination exists; use --overwrite to replace it".into(),
                    )
                    .into());
                }
                Some(transfer::read(file)?)
            } else {
                None
            };
            transfer::export(file, &data, expected.as_deref())?;
            Ok(Reply {
                data: json!({"path":file,"count":definitions.len()}),
                text: format!(
                    "Exported {} definitions to {}\n",
                    definitions.len(),
                    output::safe(&file.to_string_lossy())
                ),
            })
        }
        Command::Events { after, limit } => {
            let events = service.events_after(*after, *limit)?;
            Ok(Reply {
                data: json!({"events":events,"next_after":events.last().map_or(*after, |e| e.id)}),
                text: if events.is_empty() {
                    "No events.\n".into()
                } else {
                    events
                        .iter()
                        .map(|e| {
                            format!(
                                "{}\t{}\t{}\t{}\n",
                                e.id,
                                e.occurred_at_ms,
                                e.kind,
                                output::safe(&e.message)
                            )
                        })
                        .collect()
                },
            })
        }
        Command::Stats => stats(service),
        Command::Remediate { .. } | Command::Breakdown { .. } => {
            Err(Failure::usage("proposal command was not dispatched"))
        }
    }
}

fn edit(service: &mut Service, input: &Target, pending: bool, changes: &Changes) -> Result<Reply> {
    let (task, target) = target(service, input)?;
    let selected = task.definition();
    let revision = if pending {
        task.revisions
            .iter()
            .rev()
            .find(|r| {
                r.lifecycle == "draft"
                    && (selected.lifecycle != "ready" || r.number > selected.number)
            })
            .ok_or_else(|| Failure::missing("Task has no pending draft"))?
    } else {
        selected
    };
    let mut d = revision.definition.clone();
    if let Some(title) = &changes.title {
        d.title.clone_from(title);
    }
    if let Some(goal) = &changes.goal {
        d.goal.clone_from(goal);
    }
    if let Some(k) = changes.kind {
        d.kind = kind(k);
    }
    if !changes.criterion.is_empty() {
        d.criteria.clone_from(&changes.criterion);
    }
    changes.settings.apply(&mut d)?;
    let ready = changes.approval.map_or(revision.lifecycle == "ready", |a| {
        matches!(a, Approval::Ready)
    });
    service.revise(target, &d, ready)?;
    Ok(receipt("revised", target))
}
fn stats(service: &mut Service) -> Result<Reply> {
    let snapshot = service.snapshot()?;
    let all: Vec<_> = snapshot.queue.iter().chain(&snapshot.history).collect();
    let attempts: Vec<_> = all.iter().flat_map(|t| &t.attempts).collect();
    let succeeded = all
        .iter()
        .filter(|t| matches!(t.lifecycle.as_str(), "succeeded" | "succeeded_override"))
        .count();
    let failed = attempts.iter().filter(|a| a.state == "failed").count();
    let active_ms: i64 = attempts.iter().map(|a| a.active_ms).sum();
    let cost = attempts
        .iter()
        .filter_map(|a| a.cost_microusd)
        .reduce(|a, b| a + b);
    Ok(Reply {
        data: json!({"queue":snapshot.queue.len(),"history":snapshot.history.len(),"succeeded":succeeded,"attempts":attempts.len(),"failed_attempts":failed,"active_ms":active_ms,"recorded_cost_microusd":cost}),
        text: format!(
            "Queue: {}\nHistory: {}\nSucceeded: {succeeded}\nAttempts: {}\nFailed attempts: {failed}\nActive ms: {active_ms}\nRecorded cost (micro-USD): {}\n",
            snapshot.queue.len(),
            snapshot.history.len(),
            attempts.len(),
            cost.map_or("not recorded".into(), |n| n.to_string())
        ),
    })
}
