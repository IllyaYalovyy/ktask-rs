//! Every setting's description — how its current value and default are read from
//! [`Settings`], and how a new value for it is checked and applied — and the parsing each
//! description's `set` uses. [`super::show_settings`] and [`super::set_setting`] are the only
//! two readers of [`setting_specs`]: this is where a setting is described, once.

use std::path::Path;

use crate::Git;

use super::{
    ATTEMPT_TIMEOUT, DEFAULT_MAX_ATTEMPTS, DEFAULT_RESOLVER_PROVIDER, HEALTH_CHECK, MAX_ATTEMPTS,
    RESOLVER_MODEL, RESOLVER_PROVIDER, STEP_COMMIT, STEP_HEALTH_CHECK, STEP_PUSH, STEP_REVIEW,
    STEP_SYNC, STEP_TESTING, SetSettingError, Settings, TRACKED_BRANCH, split_tracked_branch,
    step_enabled,
};

/// `"on"` or `"off"`, as a step's own switch setting shows it.
fn toggle_value(enabled: bool) -> String {
    (if enabled { "on" } else { "off" }).to_owned()
}

/// The value [`super::show_settings`] shows for one setting, and whether it is the default.
type GetSetting = Box<dyn Fn(&Settings) -> (String, bool)>;

/// [`super::set_setting`]'s work for one setting: check a new value, apply it to [`Settings`],
/// and answer with the value to show for it. The `&dyn Git` and `&Path` are unused except by
/// [`TRACKED_BRANCH`].
type SetSetting =
    Box<dyn Fn(&mut Settings, &str, &dyn Git, &Path) -> Result<String, SetSettingError>>;

/// One setting's whole description: its name, how its current value and default are read
/// from [`Settings`], and how a new value for it is checked and applied.
pub(super) struct SettingSpec {
    pub(super) name: &'static str,
    pub(super) get: GetSetting,
    pub(super) set: SetSetting,
}

/// Every setting's description, in the order [`super::show_settings`] lists them.
pub(super) fn setting_specs() -> Vec<SettingSpec> {
    vec![
        attempt_timeout_spec(),
        health_check_spec(),
        tracked_branch_spec(),
        step_toggle_spec(STEP_SYNC, |s| s.sync_step, |s, on| s.sync_step = Some(on)),
        step_toggle_spec(
            STEP_HEALTH_CHECK,
            |s| s.health_check_step,
            |s, on| {
                s.health_check_step = Some(on);
            },
        ),
        step_toggle_spec(
            STEP_REVIEW,
            |s| s.review_step,
            |s, on| {
                s.review_step = Some(on);
            },
        ),
        step_toggle_spec(
            STEP_TESTING,
            |s| s.testing_step,
            |s, on| {
                s.testing_step = Some(on);
            },
        ),
        commit_step_spec(),
        push_step_spec(),
        max_attempts_spec(),
        resolver_provider_spec(),
        resolver_model_spec(),
    ]
}

/// [`ATTEMPT_TIMEOUT`]'s description.
fn attempt_timeout_spec() -> SettingSpec {
    SettingSpec {
        name: ATTEMPT_TIMEOUT,
        get: Box::new(|settings| {
            (
                settings
                    .attempt_timeout_seconds
                    .unwrap_or(super::DEFAULT_ATTEMPT_TIMEOUT_SECS)
                    .to_string(),
                settings.attempt_timeout_seconds.is_none(),
            )
        }),
        set: Box::new(|settings, value, _git, _dir| {
            let seconds = parse_attempt_timeout(value)?;
            settings.attempt_timeout_seconds = Some(seconds);
            Ok(seconds.to_string())
        }),
    }
}

/// [`HEALTH_CHECK`]'s description.
fn health_check_spec() -> SettingSpec {
    SettingSpec {
        name: HEALTH_CHECK,
        get: Box::new(|settings| {
            (
                settings.health_check_command.clone().unwrap_or_default(),
                settings.health_check_command.is_none(),
            )
        }),
        set: Box::new(|settings, value, _git, _dir| {
            let command = parse_health_check(value)?;
            settings.health_check_command = Some(command.clone());
            Ok(command)
        }),
    }
}

/// [`TRACKED_BRANCH`]'s description.
fn tracked_branch_spec() -> SettingSpec {
    SettingSpec {
        name: TRACKED_BRANCH,
        get: Box::new(|settings| {
            (
                settings.tracked_branch.clone().unwrap_or_default(),
                settings.tracked_branch.is_none(),
            )
        }),
        set: Box::new(|settings, value, git, project_dir| {
            let branch = parse_tracked_branch(git, project_dir, value)?;
            settings.tracked_branch = Some(branch.clone());
            Ok(branch)
        }),
    }
}

/// A plain on/off step switch's description: `field` reads its current value from
/// [`Settings`], `apply` writes a new one back. [`STEP_COMMIT`] and [`STEP_PUSH`] are not
/// plain switches — each is refused depending on the other's value — so they have their own
/// descriptions instead.
fn step_toggle_spec(
    name: &'static str,
    field: impl Fn(&Settings) -> Option<bool> + Copy + 'static,
    apply: impl Fn(&mut Settings, bool) + 'static,
) -> SettingSpec {
    SettingSpec {
        name,
        get: Box::new(move |settings| {
            (
                toggle_value(step_enabled(field(settings))),
                field(settings).is_none(),
            )
        }),
        set: Box::new(move |settings, value, _git, _dir| {
            let on = parse_step_toggle(name, value)?;
            apply(settings, on);
            Ok(toggle_value(on))
        }),
    }
}

/// [`STEP_COMMIT`]'s description: refused off while [`STEP_PUSH`] is on.
fn commit_step_spec() -> SettingSpec {
    SettingSpec {
        name: STEP_COMMIT,
        get: Box::new(|settings| {
            (
                toggle_value(step_enabled(settings.commit_step)),
                settings.commit_step.is_none(),
            )
        }),
        set: Box::new(|settings, value, _git, _dir| {
            let on = parse_step_toggle(STEP_COMMIT, value)?;
            if !on && step_enabled(settings.push_step) {
                return Err(SetSettingError::InvalidValue {
                    name: STEP_COMMIT,
                    message: "cannot switch off while push is on: the push step needs the \
                              commit step's commit; switch push off first"
                        .to_owned(),
                });
            }
            settings.commit_step = Some(on);
            Ok(toggle_value(on))
        }),
    }
}

/// [`STEP_PUSH`]'s description: refused on while [`STEP_COMMIT`] is off.
fn push_step_spec() -> SettingSpec {
    SettingSpec {
        name: STEP_PUSH,
        get: Box::new(|settings| {
            (
                toggle_value(step_enabled(settings.push_step)),
                settings.push_step.is_none(),
            )
        }),
        set: Box::new(|settings, value, _git, _dir| {
            let on = parse_step_toggle(STEP_PUSH, value)?;
            if on && !step_enabled(settings.commit_step) {
                return Err(SetSettingError::InvalidValue {
                    name: STEP_PUSH,
                    message: "cannot switch on while commit is off: the push step needs the \
                              commit step's commit; switch commit on first"
                        .to_owned(),
                });
            }
            settings.push_step = Some(on);
            Ok(toggle_value(on))
        }),
    }
}

/// [`MAX_ATTEMPTS`]'s description.
fn max_attempts_spec() -> SettingSpec {
    SettingSpec {
        name: MAX_ATTEMPTS,
        get: Box::new(|settings| {
            (
                settings
                    .max_attempts
                    .unwrap_or(DEFAULT_MAX_ATTEMPTS)
                    .to_string(),
                settings.max_attempts.is_none(),
            )
        }),
        set: Box::new(|settings, value, _git, _dir| {
            let attempts = parse_max_attempts(value)?;
            settings.max_attempts = Some(attempts);
            Ok(attempts.to_string())
        }),
    }
}

/// [`RESOLVER_PROVIDER`]'s description.
fn resolver_provider_spec() -> SettingSpec {
    SettingSpec {
        name: RESOLVER_PROVIDER,
        get: Box::new(|settings| {
            (
                settings
                    .resolver_provider
                    .clone()
                    .unwrap_or_else(|| DEFAULT_RESOLVER_PROVIDER.to_owned()),
                settings.resolver_provider.is_none(),
            )
        }),
        set: Box::new(|settings, value, _git, _dir| {
            let provider = parse_non_empty(RESOLVER_PROVIDER, value)?;
            settings.resolver_provider = Some(provider.clone());
            Ok(provider)
        }),
    }
}

/// [`RESOLVER_MODEL`]'s description.
fn resolver_model_spec() -> SettingSpec {
    SettingSpec {
        name: RESOLVER_MODEL,
        get: Box::new(|settings| {
            (
                settings.resolver_model.clone().unwrap_or_default(),
                settings.resolver_model.is_none(),
            )
        }),
        set: Box::new(|settings, value, _git, _dir| {
            let model = parse_non_empty(RESOLVER_MODEL, value)?;
            settings.resolver_model = Some(model.clone());
            Ok(model)
        }),
    }
}

/// The attempt-timeout part of [`super::set_setting`]: `value` parsed as a positive whole
/// number of seconds, or why it was refused.
fn parse_attempt_timeout(value: &str) -> Result<u64, SetSettingError> {
    let seconds: u64 = value
        .trim()
        .parse()
        .map_err(|_| SetSettingError::InvalidValue {
            name: ATTEMPT_TIMEOUT,
            message: format!("{value:?} is not a whole number of seconds"),
        })?;
    if seconds == 0 {
        return Err(SetSettingError::InvalidValue {
            name: ATTEMPT_TIMEOUT,
            message: "must be at least 1 second".to_owned(),
        });
    }
    Ok(seconds)
}

/// The health-check part of [`super::set_setting`]: `value` trimmed, or why it was refused.
fn parse_health_check(value: &str) -> Result<String, SetSettingError> {
    let command = value.trim();
    if command.is_empty() {
        return Err(SetSettingError::InvalidValue {
            name: HEALTH_CHECK,
            message: "must not be empty".to_owned(),
        });
    }
    Ok(command.to_owned())
}

/// The max-attempts part of [`super::set_setting`]: `value` parsed as a whole number of at
/// least 1, or why it was refused.
fn parse_max_attempts(value: &str) -> Result<u32, SetSettingError> {
    let attempts: u32 = value
        .trim()
        .parse()
        .map_err(|_| SetSettingError::InvalidValue {
            name: MAX_ATTEMPTS,
            message: format!("{value:?} is not a whole number"),
        })?;
    if attempts == 0 {
        return Err(SetSettingError::InvalidValue {
            name: MAX_ATTEMPTS,
            message: "must be at least 1".to_owned(),
        });
    }
    Ok(attempts)
}

/// `name`'s part of [`super::set_setting`] for a setting that is just a trimmed, non-empty
/// string: [`RESOLVER_PROVIDER`] and [`RESOLVER_MODEL`].
fn parse_non_empty(name: &'static str, value: &str) -> Result<String, SetSettingError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(SetSettingError::InvalidValue {
            name,
            message: "must not be empty".to_owned(),
        });
    }
    Ok(value.to_owned())
}

/// A step switch's part of [`super::set_setting`]: `value` read as `"on"` or `"off"`, or why
/// it was refused.
fn parse_step_toggle(name: &'static str, value: &str) -> Result<bool, SetSettingError> {
    match value.trim() {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(SetSettingError::InvalidValue {
            name,
            message: format!("{value:?} must be \"on\" or \"off\""),
        }),
    }
}

/// The tracked-branch part of [`super::set_setting`]: `value` trimmed and confirmed to name a
/// real remote branch of the repository at `project_dir`, or why it was refused.
fn parse_tracked_branch(
    git: &dyn Git,
    project_dir: &Path,
    value: &str,
) -> Result<String, SetSettingError> {
    let value = value.trim();
    let Some((remote, branch)) = split_tracked_branch(value) else {
        return Err(SetSettingError::InvalidValue {
            name: TRACKED_BRANCH,
            message: format!("{value:?} must name a remote and a branch, like \"origin/main\""),
        });
    };
    let exists = git
        .remote_branch_exists(project_dir, remote, branch)
        .map_err(SetSettingError::Git)?;
    if !exists {
        return Err(SetSettingError::InvalidValue {
            name: TRACKED_BRANCH,
            message: format!("{value:?} does not name an existing remote branch"),
        });
    }
    Ok(value.to_owned())
}
