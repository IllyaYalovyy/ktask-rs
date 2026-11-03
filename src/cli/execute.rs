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
            && self.task_limit.is_none()
            && self.stage_limit.is_none()
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
        if let Some(limit) = self.timeout_ms.or(self.task_limit) {
            d.timeout_ms = limit.0;
        }
        if let Some(limit) = self.stage_limit {
            d.stage_timeout_ms = limit.0;
        }
        d.validate()?;
        Ok(())
    }
}

fn kind(kind: Kind) -> app::Kind {
    match kind {
        Kind::Ai => app::Kind::Ai,
        Kind::Human => app::Kind::Human,
        Kind::Docs => app::Kind::Docs,
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
/// A receipt for Stop or Mark done: the task's version after its worker stopped, and the
/// stash that holds the attempt's uncommitted changes, if it left any.
fn saved_receipt(
    service: &mut Service,
    action: &str,
    id: i64,
    stash: Option<String>,
) -> Result<Reply> {
    let version = task(service, id)?.version;
    let mut text = format!("{action}: #{id} v{version}\n");
    if let Some(stash) = &stash {
        text.push_str(&format!(
            "Uncommitted changes saved as git stash \"{stash}\"; git stash pop restores them\n"
        ));
    }
    Ok(Reply {
        data: json!({"action":action,"id":id,"version":version,"stash":stash}),
        text,
    })
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
/// Task summaries; `show` loads one task with its output.
fn tasks(service: &mut Service, scope: Scope) -> Result<Vec<Task>> {
    let snapshot = service.snapshot_brief()?;
    Ok(match scope {
        Scope::Queue => snapshot.queue,
        Scope::History => snapshot.history,
        Scope::All => snapshot.queue.into_iter().chain(snapshot.history).collect(),
    })
}

/// Registers (already done by the caller), configures the check and reports readiness.
fn init(
    service: &mut Service,
    check_command: Option<&str>,
    check_name: &str,
    timeout_ms: i64,
    agent_network: Option<Switch>,
    check_before_task: Option<Switch>,
) -> Result<Reply> {
    let root = service.project_root()?;
    let check_source = if let Some(command) = check_command {
        service.set_check(check_name, command, timeout_ms)?;
        Some("given".to_owned())
    } else if service.checks()?.is_empty()
        && let Some((command, file)) = crate::runner::setup::detect_check(&root)
    {
        service.set_check(check_name, &command, timeout_ms)?;
        Some(format!("detected from {file}"))
    } else {
        None
    };
    if let Some(switch) = agent_network {
        service.set_agent_network(matches!(switch, Switch::On))?;
    }
    if let Some(switch) = check_before_task {
        service.set_check_before_task(matches!(switch, Switch::On))?;
    }
    let checks = service.checks()?;
    let models: Vec<(&str, Option<String>)> = app::configuration::MODEL_ROLES
        .iter()
        .zip(service.project_defaults()?)
        .map(|(role, model)| (*role, model.map(|m| format!("{}/{}", m.provider, m.model))))
        .collect();
    let network = service.agent_network()?;
    let verify_first = service.check_before_task()?;
    let problems = crate::runner::setup::problems(service)?;
    let mut text = format!("Project {} at {}\n", service.project_name, root.display());
    for check in &checks {
        text.push_str(&format!(
            "Check {}: {}{}\n",
            output::safe(&check.name),
            output::safe(&check.command),
            check_source
                .as_deref()
                .filter(|_| check.name == check_name)
                .map_or(String::new(), |source| format!(" ({source})"))
        ));
    }
    for (role, model) in &models {
        text.push_str(&format!(
            "{role}: {}\n",
            model.as_deref().unwrap_or("not set")
        ));
    }
    text.push_str(&format!(
        "Agent network: {}\nCheck before each task: {}\n",
        if network { "on" } else { "off" },
        if verify_first { "on" } else { "off" }
    ));
    if !problems.is_empty() {
        let missing = problems
            .iter()
            .map(|p| format!("- {}: {}. Fix: {}", p.item, p.reason, p.fix))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(crate::storage::Error::Conflict(format!(
            "Setup saved, but the project cannot run tasks yet:\n{missing}\n\n{}",
            text.trim_end()
        ))
        .into());
    }
    text.push_str("Ready: add a task with ktask add, then ktask run\n");
    Ok(Reply {
        data: json!({
            "project": service.project_name, "root": root, "checks": checks,
            "check_source": check_source, "agent_network": network,
            "check_before_task": verify_first,
            "models": models.into_iter().map(|(role, model)| (role.to_owned(), json!(model)))
                .collect::<serde_json::Map<_, _>>(),
        }),
        text,
    })
}

pub(super) fn command(service: &mut Service, command: &Command) -> Result<Reply> {
    match command {
        Command::Delivery {
            mode,
            remote,
            base,
            if_version,
        } => {
            let current = service.delivery_config()?;
            if let Some(version) = if_version
                && *version != current.version
            {
                return Err(crate::storage::Error::Conflict(format!(
                    "Delivery configuration is version {}, not {}",
                    current.version, version
                ))
                .into());
            }
            let mut value = current.clone();
            if let Some(mode) = mode {
                value.mode.clone_from(mode);
            }
            if let Some(remote) = remote {
                value.remote.clone_from(remote);
            }
            if let Some(base) = base {
                value.base_branch.clone_from(base);
            }
            let saved = if value != current {
                service.save_delivery_config(&current, &value)?
            } else {
                current
            };
            Ok(Reply {
                data: json!({"delivery":saved}),
                text: format!(
                    "Delivery v{}: {} via {}/{}\n",
                    saved.version, saved.mode, saved.remote, saved.base_branch
                ),
            })
        }
        Command::Run { .. }
        | Command::Storage { .. }
        | Command::Cache { .. }
        | Command::Retention { .. } => {
            unreachable!("runner dispatch")
        }
        Command::Init {
            check_command,
            check_name,
            check_timeout_ms,
            agent_network,
            check_before_task,
        } => init(
            service,
            check_command.as_deref(),
            check_name,
            *check_timeout_ms,
            *agent_network,
            *check_before_task,
        ),
        Command::Pause => {
            let paused = crate::runner::control::pause(service)?;
            let text = match paused {
                None => "Nothing is running\n".to_owned(),
                Some(None) => "Runner stopped; no task was running\n".to_owned(),
                Some(Some(id)) => format!("#{id} paused; run resumes it\n"),
            };
            Ok(Reply {
                data: json!({"action":"paused","id":paused.flatten(),"runner_was_running":paused.is_some()}),
                text,
            })
        }
        Command::Stop {
            target: input,
            reason,
        } => {
            let (_, target) = target(service, input)?;
            let saved = crate::runner::control::stop(service, target, reason)?;
            saved_receipt(service, "stopped", target.id, saved.stash)
        }
        Command::Done {
            target: input,
            reason,
        } => {
            let (_, target) = target(service, input)?;
            let saved = crate::runner::control::mark_done(service, target, reason)?;
            saved_receipt(service, "done", target.id, saved.stash)
        }
        Command::Cancel {
            target: input,
            reason,
        } => {
            let (_, target) = target(service, input)?;
            service.cancel(target, reason)?;
            Ok(receipt("cancelled", target))
        }
        Command::Supersede {
            target: input,
            by,
            reason,
        } => {
            let (_, target) = target(service, input)?;
            service.supersede(target, by, reason)?;
            Ok(receipt("superseded", target))
        }
        Command::Copy {
            target: input,
            before,
        } => {
            let (_, target) = target(service, input)?;
            let id = service.copy_to_queue(target, *before)?;
            Ok(Reply {
                data: json!({"action":"copied","from":target.id,"tasks":[{"id":id,"version":1}]}),
                text: format!("copied #{} to the queue as #{id}\n", target.id),
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
            new_projects,
        } => {
            let model = model
                .clone()
                .or_else(|| ModelConfig::default_model(provider).map(String::from))
                .ok_or_else(|| {
                    crate::storage::Error::Conflict(format!(
                        "--model is required for provider {provider}"
                    ))
                })?;
            service.set_default(
                role,
                &ModelConfig {
                    provider: provider.clone(),
                    model: model.clone(),
                    effort: effort.clone(),
                },
                *new_projects,
            )?;
            Ok(Reply {
                data: json!({"role":role,"provider":provider,"model":model,"effort":effort,"new_projects":new_projects}),
                text: format!(
                    "Default saved for {role}{}; existing tasks unchanged\n",
                    if *new_projects {
                        " and for new projects"
                    } else {
                        ""
                    }
                ),
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
                        crate::application::search::matches(
                            t.id,
                            t.state(),
                            &t.definition().definition.title,
                            s,
                        )
                    })
            });
            Ok(Reply::list(&rows))
        }
        Command::Show { id } => Ok(Reply::task(&task(service, *id)?)),
        Command::Artifact { id } => {
            let (artifact, content) = service.open_text_artifact(*id)?;
            Ok(Reply {
                data: json!({"artifact":artifact,"content":content}),
                text: content,
            })
        }
        Command::Current => match service.snapshot_brief()?.queue.first() {
            Some(current) => Ok(Reply::task(&task(service, current.id)?)),
            None => Ok(Reply {
                data: json!(null),
                text: "No current task.\n".into(),
            }),
        },
        Command::Add(add) => {
            let mut d = Definition {
                title: add.title.clone(),
                goal: add.goal.clone(),
                kind: kind(add.kind),
                criteria: add.criterion.clone(),
                models: service.defaults()?,
                attempt_limit: Some(3),
                timeout_ms: None,
                stage_timeout_ms: None,
            };
            add.settings.apply(&mut d)?;
            Ok(created(service.create(
                &[d],
                add.before,
                matches!(add.approval, Approval::Ready),
            )?))
        }
        Command::FollowUp { .. } => unreachable!("worker callback dispatch"),
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
        Command::Breakdown { .. } => Err(Failure::usage("proposal command was not dispatched")),
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
    if changes.preview_project_defaults {
        let defaults = service.defaults()?;
        return Ok(Reply {
            data: json!({"action":"preview_project_defaults","id":target.id,"models":app::ROLES.iter().zip(defaults.iter()).map(|(role, model)| (*role, model)).collect::<std::collections::BTreeMap<_, _>>()}),
            text: app::ROLES
                .iter()
                .zip(defaults.iter())
                .map(|(role, model)| {
                    format!(
                        "{role}: {}\n",
                        model
                            .as_ref()
                            .map_or("unconfigured".to_owned(), |m| format!(
                                "{}/{} effort {}",
                                m.provider,
                                m.model,
                                m.effort.as_deref().unwrap_or("none")
                            ))
                    )
                })
                .collect(),
        });
    }
    if changes.apply_project_defaults {
        d.models = service.defaults()?;
    }
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
    let snapshot = service.snapshot_brief()?;
    let all: Vec<_> = snapshot.queue.iter().chain(&snapshot.history).collect();
    let succeeded = all
        .iter()
        .filter(|t| matches!(t.lifecycle.as_str(), "succeeded" | "succeeded_override"))
        .count();
    let timing = service.project_timing()?;
    let failed = timing.failed;
    let active_ms = timing.active_ms;
    let waiting_ms = timing.waiting_ms;
    let usage = service.project_usage()?;
    let cost = usage.cost_microusd;
    let delivered = service.delivered_by_model()?;
    let mut text = format!(
        "Queue: {}\nHistory: {}\nSucceeded: {succeeded}\nAttempts: {}\nFailed attempts: {failed}\nActive: {}\nWaiting: {}\nProject usage: {}\n{}",
        snapshot.queue.len(),
        snapshot.history.len(),
        timing.attempts,
        crate::application::limits::elapsed(active_ms),
        crate::application::limits::elapsed(waiting_ms),
        usage.label(),
        cost.map_or_else(String::new, |n| format!("Recorded cost (micro-USD): {n}\n"))
    );
    if !delivered.is_empty() {
        text.push_str("Cost to result, average per delivered task, by execution model:\n");
    }
    for model in &delivered {
        text.push_str(&format!(
            "  {}: {} task(s) · {} · {} new tokens · {} cached · {:.1} repair rounds\n",
            output::safe(&model.model),
            model.tasks,
            crate::application::limits::elapsed(model.active_ms),
            crate::application::usage_report::count(Some(model.new_tokens)),
            crate::application::usage_report::count(Some(model.cached_tokens)),
            model.repair_rounds,
        ));
    }
    Ok(Reply {
        data: json!({"queue":snapshot.queue.len(),"history":snapshot.history.len(),"succeeded":succeeded,"attempts":timing.attempts,"failed_attempts":failed,"active_ms":active_ms,"waiting_ms":waiting_ms,"recorded_cost_microusd":cost,"usage":usage,"delivered_by_model":delivered}),
        text,
    })
}
