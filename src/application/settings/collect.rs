//! A project's settings as they are now, in the shape of the file. Export writes this, and
//! import compares a file with it.
use super::file::*;
use crate::{
    application::{
        categories, configuration, fix::read_fix_limits, investigation::read_investigation_limits,
        lenses, preferences::Preferences, profiles, retention, user_defaults,
    },
    build_cache,
    storage::Result,
};
use rusqlite::{Connection, OptionalExtension};

pub(super) fn models(values: configuration::Defaults) -> Models {
    configuration::MODEL_ROLES
        .iter()
        .zip(values)
        .filter_map(|(role, model)| Some(((*role).to_owned(), model?.into())))
        .collect()
}

fn categories_of(db: &Connection, project: Option<i64>) -> Result<Vec<Category>> {
    Ok(categories::stored(db, project)?
        .into_iter()
        .map(|category| Category {
            name: category.name,
            description: category.description,
            run_checks: category.checks,
            templates: category.templates,
        })
        .collect())
}

fn lenses_of(db: &Connection, project: Option<i64>) -> Result<Vec<Lens>> {
    Ok(lenses::stored(db, project)?
        .into_iter()
        .map(|lens| Lens {
            name: lens.name,
            description: lens.description,
            instructions: lens.instructions,
        })
        .collect())
}

fn project_settings(db: &Connection, id: i64, preferences: &Preferences) -> Result<Project> {
    let (agent_network, check_before_task, profile): (bool, bool, Option<String>) = db.query_row(
        "SELECT agent_network,check_before_task,(SELECT name FROM model_profiles WHERE id=projects.profile_id) FROM projects WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let provisioning: bool = db
        .query_row(
            "SELECT enabled FROM provision_policy WHERE project_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false);
    let delivery = configuration::delivery(db, id)?;
    let retention = retention::policy(db, id)?;
    let report = read_investigation_limits(db, id)?;
    let fix = read_fix_limits(db, id)?;
    Ok(Project {
        checks: configuration::checks(db, id)?
            .into_iter()
            .map(|check| Check {
                name: check.name,
                command: check.command,
                directory: check.directory,
                timeout_ms: check.timeout_ms,
                enabled: check.enabled,
            })
            .collect(),
        models: models(configuration::defaults(db, id)?),
        profile: Some(profile),
        agent_network: Some(agent_network),
        check_before_task: Some(check_before_task),
        automatic_provisioning: Some(provisioning),
        preferences: PreferenceValues {
            mouse: Some(preferences.mouse),
            run_mode: Some(preferences.continuation),
            planner_approval: Some(preferences.planner_approval),
        },
        categories: categories_of(db, Some(id))?,
        lenses: lenses_of(db, Some(id))?,
        lens_defaults: Some(lenses::defaults_of(db, id)?),
        delivery: Delivery {
            mode: Some(delivery.mode),
            remote: Some(delivery.remote),
            base_branch: Some(delivery.base_branch),
        },
        retention: Retention {
            days: Some(retention.days),
            max_bytes: Some(retention.max_bytes),
            max_files: Some(retention.max_files),
        },
        report_limits: Limits {
            time_limit_ms: Some(report.time_limit_ms),
            cost_limit_microusd: Some(report.cost_limit_microusd),
        },
        fix_limits: Limits {
            time_limit_ms: Some(fix.time_limit_ms),
            cost_limit_microusd: Some(fix.cost_limit_microusd),
        },
    })
}

fn shared_settings(db: &Connection, cache: build_cache::Policy) -> Result<Shared> {
    let user = user_defaults::read_user_defaults(db)?;
    Ok(Shared {
        user_defaults: UserValues {
            agent_network: Some(user.agent_network),
            check_before_task: Some(user.check_before_task),
            check_timeout_ms: Some(user.check_timeout_ms),
            run_mode: Some(user.continuation),
            planner_approval: Some(user.planner_approval),
            delivery_mode: Some(user.delivery_mode),
            default_profile: Some(user.default_profile),
        },
        models: models(configuration::default_models(db)?),
        profiles: profiles::all(db)?
            .into_iter()
            .map(|profile| Profile {
                name: profile.name,
                models: models(profile.models),
            })
            .collect(),
        categories: categories_of(db, None)?,
        lenses: lenses_of(db, None)?,
        build_cache: BuildCache {
            max_bytes: Some(cache.max_bytes),
            max_files: Some(cache.max_files),
            keep_generations: Some(cache.keep_generations),
            grace_secs: Some(cache.grace_secs),
            min_free_bytes: Some(cache.min_free_bytes),
        },
    })
}

/// The tool versions bound to `project`.
pub(super) fn bound_tools(db: &Connection, project: i64) -> Result<Vec<ToolNeed>> {
    Ok(db
        .prepare("SELECT p.name,g.version FROM project_tools p JOIN tool_generations g ON g.id=p.generation_id WHERE p.project_id=?1 ORDER BY p.name")?
        .query_map([project], |r| {
            Ok(ToolNeed {
                name: r.get(0)?,
                version: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

pub(super) fn collect(
    db: &Connection,
    project: i64,
    preferences: &Preferences,
    cache: build_cache::Policy,
) -> Result<SettingsFile> {
    Ok(SettingsFile {
        project: project_settings(db, project, preferences)?,
        shared: shared_settings(db, cache)?,
        tools: bound_tools(db, project)?,
        ..SettingsFile::new()
    })
}
