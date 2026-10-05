//! The settings that bound retries of whole task attempts and interrupted Codex streams.

use crate::{DEFAULT_MAX_ATTEMPTS, DEFAULT_TRANSPORT_RETRIES, MAX_ATTEMPTS, TRANSPORT_RETRIES};

use super::{SetSettingError, SettingSpec};

/// [`MAX_ATTEMPTS`]'s description.
pub(super) fn max_attempts_spec() -> SettingSpec {
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
            let attempts = parse_positive_count(MAX_ATTEMPTS, value)?;
            settings.max_attempts = Some(attempts);
            Ok(attempts.to_string())
        }),
    }
}

/// [`TRANSPORT_RETRIES`]'s description.
pub(super) fn transport_retries_spec() -> SettingSpec {
    SettingSpec {
        name: TRANSPORT_RETRIES,
        get: Box::new(|settings| {
            (
                settings
                    .transport_retries
                    .unwrap_or(DEFAULT_TRANSPORT_RETRIES)
                    .to_string(),
                settings.transport_retries.is_none(),
            )
        }),
        set: Box::new(|settings, value, _git, _dir| {
            let retries = parse_positive_count(TRANSPORT_RETRIES, value)?;
            settings.transport_retries = Some(retries);
            Ok(retries.to_string())
        }),
    }
}

/// `value` as a whole number of at least one, for a retry limit named `name`.
fn parse_positive_count(name: &'static str, value: &str) -> Result<u32, SetSettingError> {
    let count: u32 = value
        .trim()
        .parse()
        .map_err(|_| SetSettingError::InvalidValue {
            name,
            message: format!("{value:?} is not a whole number"),
        })?;
    if count == 0 {
        return Err(SetSettingError::InvalidValue {
            name,
            message: "must be at least 1".to_owned(),
        });
    }
    Ok(count)
}
