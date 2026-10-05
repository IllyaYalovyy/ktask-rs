//! The project agent's provider and model setting specifications.

use super::SettingSpec;
use crate::{DEFAULT_PROVIDER, MODEL, PROVIDER, SetSettingError};

fn non_empty(name: &'static str, value: &str) -> Result<String, SetSettingError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(SetSettingError::InvalidValue {
            name,
            message: "must not be empty".to_owned(),
        });
    }
    Ok(value.to_owned())
}

/// [`PROVIDER`]'s description.
pub(super) fn provider_spec() -> SettingSpec {
    SettingSpec {
        name: PROVIDER,
        get: Box::new(|settings| {
            (
                settings
                    .provider
                    .clone()
                    .unwrap_or_else(|| DEFAULT_PROVIDER.to_owned()),
                settings.provider.is_none(),
            )
        }),
        set: Box::new(|settings, value, _git, _dir| {
            let provider = non_empty(PROVIDER, value)?;
            settings.provider = Some(provider.clone());
            Ok(provider)
        }),
    }
}

/// [`MODEL`]'s description.
pub(super) fn model_spec() -> SettingSpec {
    SettingSpec {
        name: MODEL,
        get: Box::new(|settings| {
            (
                settings.model.clone().unwrap_or_default(),
                settings.model.is_none(),
            )
        }),
        set: Box::new(|settings, value, _git, _dir| {
            let model = non_empty(MODEL, value)?;
            settings.model = Some(model.clone());
            Ok(model)
        }),
    }
}
