//! `ktask profile`: shared profile commands with JSON and text output.
use super::{Failure, Result, args::*, output::Reply};
use crate::application::{
    ModelConfig, Service,
    configuration::{Defaults, MODEL_ROLES},
    profiles::{self, Effective, Profile, Usage},
    user_defaults::model_text,
};
use serde_json::{Value, json};

fn models_json(models: &Defaults) -> Value {
    MODEL_ROLES
        .iter()
        .zip(models)
        .map(|(role, model)| ((*role).to_owned(), json!(model)))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

fn profile_json(profile: &Profile, usage: &Usage) -> Value {
    json!({
        "name": profile.name,
        "version": profile.version,
        "models": models_json(&profile.models),
        "used_by": usage,
    })
}

fn profile_text(profile: &Profile, usage: &Usage) -> String {
    let mut text = format!(
        "Profile {} v{} ({})\n",
        super::output::safe(&profile.name),
        profile.version,
        if usage.in_use() {
            format!("used by {}", super::output::safe(&usage.describe()))
        } else {
            "unused".into()
        }
    );
    for (role, model) in MODEL_ROLES.iter().zip(&profile.models) {
        text.push_str(&format!(
            "{}\n",
            super::output::safe(&crate::presentation::role_setting(role, model.as_ref()))
        ));
    }
    text
}

fn found(service: &Service, name: &str) -> Result<Profile> {
    service
        .profile(name)?
        .ok_or_else(|| Failure::missing(format!("Profile {name} does not exist")))
}

fn guard(profile: &Profile, if_version: Option<i64>) -> Result<()> {
    if let Some(version) = if_version
        && version != profile.version
    {
        return Err(crate::storage::Error::Conflict(format!(
            "Profile {} is version {}, not {}; read it again before retrying",
            profile.name, profile.version, version
        ))
        .into());
    }
    Ok(())
}

fn set_models(
    models: &mut Defaults,
    set: &[ProfileModel],
    efforts: &[ProfileEffort],
) -> Result<()> {
    for setting in set {
        let effort = models[setting.role].as_ref().and_then(|m| m.effort.clone());
        models[setting.role] = Some(ModelConfig {
            provider: setting.provider.clone(),
            model: setting.model.clone(),
            effort,
        });
    }
    for setting in efforts {
        let model = models[setting.role].as_mut().ok_or_else(|| {
            Failure::usage(format!(
                "Configure a {} model before setting its effort",
                MODEL_ROLES[setting.role]
            ))
        })?;
        model.effort.clone_from(&setting.effort);
    }
    Ok(())
}

fn effective_reply(scope: &str, profile: Option<&str>, rows: &[Effective]) -> Reply {
    let mut text = format!(
        "{scope}: profile {}\n",
        super::output::safe(profile.unwrap_or("none"))
    );
    for row in rows {
        text.push_str(&format!(
            "{}\t{}\t{}\n",
            row.role,
            super::output::safe(&model_text(row.model.as_ref())),
            super::output::safe(&row.source.label())
        ));
    }
    Reply {
        data: json!({"scope": scope, "profile": profile, "roles": rows}),
        text,
    }
}

fn pinned_reply(service: &mut Service, id: i64) -> Result<Reply> {
    let task = service
        .task(id)?
        .ok_or_else(|| Failure::missing(format!("Task #{id} does not exist in this project")))?;
    let revision = task.definition();
    let rows = profiles::pinned_rows(&revision.definition.models, &service.effective_models()?);
    let mut text = format!(
        "Task #{id} revision {}: models pinned when it was defined\n",
        revision.number
    );
    for row in &rows {
        text.push_str(&format!(
            "{}\t{}\t{}\n",
            row.role,
            super::output::safe(&model_text(row.pinned.as_ref())),
            if row.differs {
                format!(
                    "project now gives {} ({}); apply it with edit --apply-project-defaults",
                    super::output::safe(&model_text(row.current.as_ref())),
                    super::output::safe(&row.current_source.label())
                )
            } else {
                "matches the project's current model".into()
            }
        ));
    }
    Ok(Reply {
        data: json!({"scope": "task", "task": id, "revision": revision.number, "roles": rows}),
        text,
    })
}

pub(super) fn profile(service: &mut Service, action: &ProfileAction) -> Result<Reply> {
    match action {
        ProfileAction::List => {
            let list = service.profiles()?;
            let selected = service.selected_profile()?.map(|p| p.name);
            let default = service.user_defaults()?.default_profile;
            let mut text = String::new();
            let mut data = Vec::new();
            for profile in &list {
                let usage = service.profile_usage(profile)?;
                text.push_str(&format!(
                    "{}\tv{}\t{}{}\n",
                    super::output::safe(&profile.name),
                    profile.version,
                    if selected.as_deref() == Some(&profile.name) {
                        "selected here"
                    } else {
                        "-"
                    },
                    if default.as_deref() == Some(&profile.name) {
                        "\tnew-project default"
                    } else {
                        ""
                    }
                ));
                data.push(profile_json(profile, &usage));
            }
            if list.is_empty() {
                text.push_str("No profiles.\n");
            }
            Ok(Reply {
                data: json!({"profiles": data, "selected": selected, "default": default}),
                text,
            })
        }
        ProfileAction::Show { name } => {
            let profile = found(service, name)?;
            let usage = service.profile_usage(&profile)?;
            Ok(Reply {
                data: json!({"profile": profile_json(&profile, &usage)}),
                text: profile_text(&profile, &usage),
            })
        }
        ProfileAction::Create {
            name,
            from_project,
            model,
            effort,
            select,
            make_default,
        } => {
            let mut models: Defaults = if *from_project {
                service.project_defaults()?
            } else {
                std::array::from_fn(|_| None)
            };
            set_models(&mut models, model, effort)?;
            let created = service.create_profile(name, &models)?;
            if *select {
                service.select_profile(Some(&created.name))?;
            }
            if *make_default {
                service.set_default_profile(Some(&created.name))?;
            }
            let usage = service.profile_usage(&created)?;
            Ok(Reply {
                data: json!({"action": "created", "profile": profile_json(&created, &usage)}),
                text: format!("created\n{}", profile_text(&created, &usage)),
            })
        }
        ProfileAction::Edit {
            name,
            rename,
            model,
            effort,
            clear_model,
            if_version,
        } => {
            let current = found(service, name)?;
            guard(&current, *if_version)?;
            let mut models = current.models.clone();
            for role in clear_model {
                if model.iter().any(|m| m.role == *role) || effort.iter().any(|m| m.role == *role) {
                    return Err(Failure::usage(format!(
                        "Duplicate/conflicting settings for {}",
                        MODEL_ROLES[*role]
                    )));
                }
                models[*role] = None;
            }
            set_models(&mut models, model, effort)?;
            let saved = service.save_profile(
                &current,
                rename.as_deref().unwrap_or(&current.name),
                &models,
            )?;
            let usage = service.profile_usage(&saved)?;
            Ok(Reply {
                data: json!({"action": "saved", "profile": profile_json(&saved, &usage)}),
                text: format!(
                    "saved; only tasks defined later use it\n{}",
                    profile_text(&saved, &usage)
                ),
            })
        }
        ProfileAction::Delete { name, if_version } => {
            let current = found(service, name)?;
            guard(&current, *if_version)?;
            service.delete_profile(&current)?;
            Ok(Reply {
                data: json!({"action": "deleted", "name": current.name}),
                text: format!("deleted profile {}\n", super::output::safe(&current.name)),
            })
        }
        ProfileAction::Select { name, none } => {
            let chosen = service.select_profile(if *none { None } else { name.as_deref() })?;
            Ok(Reply {
                data: json!({"action": "selected", "profile": chosen.as_ref().map(|p| &p.name)}),
                text: match &chosen {
                    Some(profile) => format!(
                        "this project uses profile {}; only tasks defined later change\n",
                        super::output::safe(&profile.name)
                    ),
                    None => "this project uses the user defaults without a profile\n".into(),
                },
            })
        }
        ProfileAction::Default { name, none } => {
            let saved = service.set_default_profile(if *none { None } else { name.as_deref() })?;
            Ok(Reply {
                data: json!({"action": "default", "profile": saved.default_profile}),
                text: match &saved.default_profile {
                    Some(name) => format!(
                        "new projects start with profile {}\n",
                        super::output::safe(name)
                    ),
                    None => "new projects start without a profile\n".into(),
                },
            })
        }
        ProfileAction::Effective { task, new_project } => {
            if let Some(id) = task {
                return pinned_reply(service, *id);
            }
            if *new_project {
                let name = service.user_defaults()?.default_profile;
                return Ok(effective_reply(
                    "new_project",
                    name.as_deref(),
                    &service.new_project_effective()?,
                ));
            }
            let name = service.selected_profile()?.map(|p| p.name);
            Ok(effective_reply(
                "project",
                name.as_deref(),
                &service.effective_models()?,
            ))
        }
        ProfileAction::Inherit { roles, all } => {
            if !*all && roles.is_empty() {
                return Err(Failure::usage("Name the roles to clear, or pass --all"));
            }
            let current = service.project_overrides()?;
            let mut values = current.clone();
            let cleared: Vec<&str> = (0..MODEL_ROLES.len())
                .filter(|index| *all || roles.contains(index))
                .map(|index| {
                    values[index] = None;
                    MODEL_ROLES[index]
                })
                .collect();
            service.save_project_overrides(&current, &values)?;
            Ok(effective_reply(
                "project",
                service
                    .selected_profile()?
                    .as_ref()
                    .map(|p| p.name.as_str()),
                &service.effective_models()?,
            )
            .with_note(&format!("cleared overrides: {}", cleared.join(", "))))
        }
    }
}
