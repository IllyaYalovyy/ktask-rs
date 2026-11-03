//! Structural model validation only: ktask never judges a model's fitness or existence.
//! Any nonempty model string for a supported provider is the owner's choice and reaches the
//! provider CLI unchanged; the provider reports names it does not accept when it runs.
use super::*;

impl Service {
    /// Provider is codex, claude or opencode; provider and model are nonempty; an explicit
    /// effort uses a spelling the provider accepts.
    pub fn validate_model(&self, role: &str, model: &ModelConfig) -> Result<()> {
        model.validate_for_role(role)?;
        if let Some(effort) = &model.effort {
            let accepted = ModelConfig::accepted_efforts(&model.provider).unwrap_or_default();
            require(
                accepted.contains(&effort.as_str()),
                &format!(
                    "{role} effort {effort:?} is invalid\naccepted values: {}\nModel {}/{}",
                    accepted.join(", "),
                    model.provider,
                    model.model,
                ),
            )?;
        }
        Ok(())
    }

    pub fn validate_definition_models(&self, definition: &Definition) -> Result<()> {
        for (role, model) in ROLES.iter().zip(&definition.models) {
            if let Some(model) = model {
                self.validate_model(role, model)?;
            }
        }
        Ok(())
    }
}
