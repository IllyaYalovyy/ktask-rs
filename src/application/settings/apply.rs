//! Writes the database settings a file names, under the caller's transaction. Every value is
//! validated by the rule its own command uses; only values that differ are written, so
//! applying a file again changes nothing. Whatever the file does not name stays.
use super::{collect, file::*};
use crate::{
    application::{
        Actor, ActorKind, ModelConfig, categories,
        configuration::{self, MODEL_ROLES, ProjectCheck},
        fix::{read_fix_limits, validate_fix_limits},
        investigation::{
            InvestigationLimits, read_investigation_limits, validate_investigation_limits,
            write_agent_limits,
        },
        lenses,
        model_validation::check_model,
        now_ms, profiles, require, retention, user_defaults,
    },
    storage::{Error, Result},
};
use rusqlite::{Connection, Transaction, params};
use std::path::Path;

/// The project being imported into.
pub(super) struct Project<'a> {
    pub id: i64,
    pub root: &'a Path,
    pub actor: &'a Actor,
}

/// Shared settings come first: the project's selections name them.
pub(super) fn apply(
    tx: &Transaction<'_>,
    project: &Project<'_>,
    file: &SettingsFile,
) -> Result<()> {
    shared(tx, &file.shared)?;
    let values = &file.project;
    overrides(tx, project.id, &values.models)?;
    if let Some(name) = &values.profile {
        select_profile(tx, project.id, name.as_deref())?;
    }
    switches(tx, project, values)?;
    definitions(tx, Some(project.id), &values.categories, &values.lenses)?;
    if let Some(names) = &values.lens_defaults {
        default_lenses(tx, project.id, names)?;
    }
    checks(tx, project, &values.checks)?;
    delivery(tx, project.id, &values.delivery)?;
    storage(tx, project.id, &values.retention)?;
    agent_limits(tx, project.id, values)
}

fn shared(tx: &Transaction<'_>, shared: &Shared) -> Result<()> {
    for profile in &shared.profiles {
        merge_profile(tx, profile)?;
    }
    definitions(tx, None, &shared.categories, &shared.lenses)?;
    user_settings(tx, shared)
}

/// The models of `values` by role, each checked, over the ones `current` has.
fn merged(current: &configuration::Defaults, values: &Models) -> Result<configuration::Defaults> {
    let mut result = current.clone();
    for (role, slot) in MODEL_ROLES.iter().zip(result.iter_mut()) {
        if let Some(model) = values.get(*role) {
            let model = ModelConfig::from(model);
            check_model(role, &model)?;
            *slot = Some(model);
        }
    }
    Ok(result)
}

/// Creates the profile or sets the roles the file names on it.
fn merge_profile(tx: &Transaction<'_>, profile: &Profile) -> Result<()> {
    let name = profiles::valid_name(&profile.name)?;
    let existing = profiles::profile_by_name(tx, name)?;
    let current = existing.as_ref().map_or_else(
        || std::array::from_fn(|_| None),
        |profile| profile.models.clone(),
    );
    let models = merged(&current, &profile.models)?;
    require(
        models.iter().any(Option::is_some),
        &format!("Profile {name} needs at least one role model"),
    )?;
    let id = match existing {
        Some(_) if models == current => return Ok(()),
        Some(found) => {
            tx.execute(
                "UPDATE model_profiles SET row_version=row_version+1,updated_at_ms=?2 WHERE id=?1",
                params![found.id, now_ms()],
            )?;
            tx.execute(
                "DELETE FROM model_profile_roles WHERE profile_id=?1",
                [found.id],
            )?;
            found.id
        }
        None => {
            tx.execute(
                "INSERT INTO model_profiles(name,created_at_ms,updated_at_ms) VALUES (?1,?2,?2)",
                params![name, now_ms()],
            )?;
            tx.last_insert_rowid()
        }
    };
    profiles::write_roles(tx, id, &models)
}

fn profile_named(db: &Connection, name: &str) -> Result<profiles::Profile> {
    profiles::profile_by_name(db, name)?
        .ok_or_else(|| Error::Conflict(format!("Unknown profile {name}")))
}

/// The user defaults and user models share one version, advanced once.
fn user_settings(tx: &Transaction<'_>, shared: &Shared) -> Result<()> {
    let current = user_defaults::read_user_defaults(tx)?;
    let values = &shared.user_defaults;
    let mut next = current.clone();
    next.agent_network = values.agent_network.unwrap_or(next.agent_network);
    next.check_before_task = values.check_before_task.unwrap_or(next.check_before_task);
    next.check_timeout_ms = values.check_timeout_ms.unwrap_or(next.check_timeout_ms);
    next.continuation = values.run_mode.unwrap_or(next.continuation);
    next.planner_approval = values.planner_approval.unwrap_or(next.planner_approval);
    if let Some(mode) = &values.delivery_mode {
        next.delivery_mode.clone_from(mode);
    }
    if let Some(profile) = &values.default_profile {
        next.default_profile = profile
            .as_deref()
            .map(|name| profile_named(tx, name).map(|found| found.name))
            .transpose()?;
    }
    user_defaults::validate_user_defaults(&next)?;
    let current_models = configuration::default_models(tx)?;
    let models = merged(&current_models, &shared.models)?;
    if next == current && models == current_models {
        return Ok(());
    }
    let version = user_defaults::next_version(current.version)?;
    user_defaults::write_user_defaults(tx, version, &next)?;
    configuration::replace_default_models(tx, &models)
}

/// Categories and lenses of one layer: the project's, or the user scope with `None`.
fn definitions(
    tx: &Transaction<'_>,
    layer: Option<i64>,
    new_categories: &[Category],
    new_lenses: &[Lens],
) -> Result<()> {
    let scope = if layer.is_some() {
        categories::Scope::Project
    } else {
        categories::Scope::User
    };
    let current = categories::stored(tx, layer)?;
    for value in new_categories {
        let category = categories::Category {
            name: value.name.clone(),
            description: value.description.clone(),
            checks: value.run_checks,
            templates: value.templates.clone(),
            scope,
        };
        let same = current.iter().any(|found| {
            found.name.eq_ignore_ascii_case(&category.name)
                && found.description == category.description
                && found.checks == category.checks
                && found.templates == category.templates
        });
        if !same {
            categories::put(tx, layer, &category)?;
        }
    }
    let current = lenses::stored(tx, layer)?;
    for value in new_lenses {
        let lens = lenses::Lens {
            name: value.name.clone(),
            description: value.description.clone(),
            instructions: value.instructions.clone(),
            scope,
        };
        let same = current.iter().any(|found| {
            found.name.eq_ignore_ascii_case(&lens.name)
                && found.description == lens.description
                && found.instructions == lens.instructions
        });
        if !same {
            lenses::put(tx, layer, &lens)?;
        }
    }
    Ok(())
}

fn overrides(tx: &Transaction<'_>, project: i64, values: &Models) -> Result<()> {
    let current = configuration::defaults(tx, project)?;
    let next = merged(&current, values)?;
    for ((role, old), new) in MODEL_ROLES.iter().zip(&current).zip(&next) {
        if let Some(model) = new
            && new != old
        {
            configuration::write_override(tx, project, role, model)?;
        }
    }
    Ok(())
}

fn select_profile(tx: &Transaction<'_>, project: i64, name: Option<&str>) -> Result<()> {
    let id = name
        .map(|name| profile_named(tx, name).map(|profile| profile.id))
        .transpose()?;
    tx.execute(
        "UPDATE projects SET profile_id=?2 WHERE id=?1 AND profile_id IS NOT ?2",
        params![project, id],
    )?;
    Ok(())
}

fn switches(
    tx: &Transaction<'_>,
    project: &Project<'_>,
    values: &super::file::Project,
) -> Result<()> {
    tx.execute(
        "UPDATE projects SET agent_network=coalesce(?2,agent_network),check_before_task=coalesce(?3,check_before_task) WHERE id=?1",
        params![project.id, values.agent_network, values.check_before_task],
    )?;
    let Some(enabled) = values.automatic_provisioning else {
        return Ok(());
    };
    let current: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM provision_policy WHERE project_id=?1 AND enabled=1)",
        [project.id],
        |r| r.get(0),
    )?;
    if current != enabled {
        require(
            matches!(project.actor.kind, ActorKind::Human),
            "Only a human actor may enable provisioning",
        )?;
        tx.execute("INSERT INTO provision_policy(project_id,enabled) VALUES (?1,?2) ON CONFLICT(project_id) DO UPDATE SET enabled=excluded.enabled,row_version=row_version+1", params![project.id,enabled])?;
    }
    Ok(())
}

fn default_lenses(tx: &Transaction<'_>, project: i64, names: &[String]) -> Result<()> {
    let current = lenses::defaults_of(tx, project)?;
    let same = current.len() == names.len()
        && current
            .iter()
            .zip(names)
            .all(|(old, new)| old.eq_ignore_ascii_case(new.trim()));
    if !same {
        let names: Vec<String> = names.iter().map(|name| name.trim().to_owned()).collect();
        lenses::write_defaults(tx, project, &names)?;
    }
    Ok(())
}

/// Checks are matched by name; new ones follow the existing ones in file order.
fn checks(tx: &Transaction<'_>, project: &Project<'_>, values: &[Check]) -> Result<()> {
    let current = configuration::checks(tx, project.id)?;
    for value in values {
        let existing = current.iter().find(|check| check.name == value.name);
        let check = ProjectCheck {
            id: existing.and_then(|check| check.id),
            name: value.name.clone(),
            command: value.command.clone(),
            directory: value.directory.clone(),
            timeout_ms: value.timeout_ms,
            enabled: value.enabled,
        };
        configuration::validate_check(project.root, &check).map_err(|error| {
            Error::Conflict(format!(
                "Check {} ({}): {error}",
                check.name, check.directory
            ))
        })?;
        if existing != Some(&check) {
            configuration::write_check(tx, project.id, &check)?;
        }
    }
    Ok(())
}

fn delivery(tx: &Transaction<'_>, project: i64, values: &Delivery) -> Result<()> {
    let current = configuration::delivery(tx, project)?;
    let mut next = current.clone();
    if let Some(mode) = &values.mode {
        next.mode.clone_from(mode);
    }
    if let Some(remote) = &values.remote {
        next.remote.clone_from(remote);
    }
    if let Some(base) = &values.base_branch {
        next.base_branch.clone_from(base);
    }
    configuration::validate_delivery(&next)?;
    if next != current {
        configuration::write_delivery(tx, project, current.version + 1, &next)?;
    }
    Ok(())
}

fn storage(tx: &Transaction<'_>, project: i64, values: &Retention) -> Result<()> {
    let current = retention::policy(tx, project)?;
    let mut next = current;
    next.days = values.days.unwrap_or(next.days);
    next.max_bytes = values.max_bytes.unwrap_or(next.max_bytes);
    next.max_files = values.max_files.unwrap_or(next.max_files);
    retention::validate_policy(next)?;
    if next != current {
        next.version += 1;
        retention::write_policy(tx, project, next)?;
    }
    Ok(())
}

fn limits(current: &InvestigationLimits, values: &Limits) -> InvestigationLimits {
    InvestigationLimits {
        version: current.version,
        time_limit_ms: values.time_limit_ms.unwrap_or(current.time_limit_ms),
        cost_limit_microusd: values
            .cost_limit_microusd
            .unwrap_or(current.cost_limit_microusd),
    }
}

/// Report and fix limits share one row and version.
fn agent_limits(tx: &Transaction<'_>, project: i64, values: &super::file::Project) -> Result<()> {
    let report = read_investigation_limits(tx, project)?;
    let fix = read_fix_limits(tx, project)?;
    let next_report = limits(&report, &values.report_limits);
    let next_fix = limits(&fix, &values.fix_limits);
    validate_investigation_limits(next_report.time_limit_ms, next_report.cost_limit_microusd)?;
    validate_fix_limits(next_fix.time_limit_ms, next_fix.cost_limit_microusd)?;
    if next_report != report || next_fix != fix {
        write_agent_limits(tx, project, &next_report, &next_fix, report.version + 1)?;
    }
    Ok(())
}

/// Every model the settings or the project would use, each with where it is used.
pub(super) fn needed_models(
    db: &Connection,
    project: i64,
    file: &SettingsFile,
) -> Result<Vec<(String, ModelConfig)>> {
    let mut found: Vec<(String, ModelConfig)> = profiles::resolve(db, Some(project))?
        .into_iter()
        .filter_map(|row| Some((row.role.to_owned(), row.model?)))
        .collect();
    let in_profiles = file.shared.profiles.iter().flat_map(|profile| {
        profile
            .models
            .iter()
            .map(|(role, model)| (format!("profile {} {role}", profile.name), model.into()))
    });
    found.extend(in_profiles);
    for (role, model) in collect::models(configuration::default_models(db)?) {
        found.push((format!("user default {role}"), (&model).into()));
    }
    Ok(found)
}
