//! Exact provider IDs and explicitly configured aliases; names never imply capabilities.
use super::*;
use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    provider: String,
    model: String,
    #[serde(default)]
    aliases: Vec<String>,
    /// Omitted means the provider-wide effort spellings apply.
    efforts: Option<Vec<String>>,
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Conflict(format!(
            "Cannot read model metadata {}: {error}",
            path.display()
        ))),
    }
}

impl Service {
    /// Save and execution preflight use the same metadata, scoped to this explicit instance.
    pub fn validate_model(&self, role: &str, model: &ModelConfig) -> Result<()> {
        self.model_identity(role, model).map(|_| ())
    }

    fn model_identity(&self, role: &str, model: &ModelConfig) -> Result<(String, String)> {
        model.validate_for_role(role)?;
        let context = || format!("{role} model {}/{}", model.provider, model.model);
        let invalid = |message: String| Error::Conflict(format!("{}: {message}", context()));
        let configured = self
            .db
            .state_file("model-catalog.json")
            .map_err(|error| invalid(format!("cannot locate model metadata: {error}")))?;
        require(
            matches!(model.provider.as_str(), "codex" | "claude" | "opencode"),
            &format!(
                "{}: unsupported provider; use codex, claude or opencode",
                context()
            ),
        )?;
        if let Some(text) = read(&configured).map_err(|error| invalid(error.to_string()))? {
            let entries: Vec<Entry> = serde_json::from_str(&text).map_err(|error| {
                invalid(format!(
                    "invalid metadata {}: {error}; fix the catalog",
                    configured.display()
                ))
            })?;
            for entry in &entries {
                if entry.provider.trim().is_empty()
                    || entry.model.trim().is_empty()
                    || entry.aliases.iter().any(|alias| alias.trim().is_empty())
                    || entry
                        .efforts
                        .as_ref()
                        .is_some_and(|values| values.iter().any(|value| value.trim().is_empty()))
                {
                    return Err(invalid(format!(
                        "invalid metadata {}; entries need nonempty provider, model, aliases and efforts",
                        configured.display()
                    )));
                }
            }
            let mut identities = BTreeMap::new();
            for entry in &entries {
                for name in std::iter::once(&entry.model).chain(&entry.aliases) {
                    let key = (&entry.provider, name);
                    let identity = (&entry.model, &entry.efforts);
                    if identities
                        .insert(key, identity)
                        .is_some_and(|old| old != identity)
                    {
                        return Err(invalid(format!(
                            "ambiguous metadata {}: {}/{} maps to conflicting models or efforts; fix the catalog",
                            configured.display(),
                            entry.provider,
                            name
                        )));
                    }
                }
            }
            if let Some(entry) = entries.iter().find(|entry| {
                entry.provider == model.provider
                    && (entry.model == model.model || entry.aliases.contains(&model.model))
            }) {
                validate_effort(role, model, entry.efforts.as_deref(), &configured)?;
                return Ok((entry.provider.clone(), entry.model.clone()));
            }
        }
        if model.provider == "claude" && ["sonnet", "opus", "haiku"].contains(&model.model.as_str())
        {
            validate_effort(role, model, None, &configured)?;
            return Ok((model.provider.clone(), model.model.clone()));
        }
        if model.provider == "codex" {
            let home = std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex"))
                });
            if let Some(path) = home.map(|home| home.join("models_cache.json"))
                && let Some(text) = read(&path).map_err(|error| invalid(error.to_string()))?
            {
                #[derive(Deserialize)]
                struct Cache {
                    models: Vec<CodexModel>,
                }
                #[derive(Deserialize)]
                struct CodexModel {
                    slug: String,
                    supported_reasoning_levels: Vec<Level>,
                }
                #[derive(Deserialize)]
                struct Level {
                    effort: String,
                }
                let cache: Cache = serde_json::from_str(&text).map_err(|error| {
                    invalid(format!("invalid metadata {}: {error}; refresh Codex model metadata or configure {}", path.display(), configured.display()))
                })?;
                if let Some(entry) = cache.models.iter().find(|entry| entry.slug == model.model) {
                    let efforts: Vec<_> = entry
                        .supported_reasoning_levels
                        .iter()
                        .map(|level| level.effort.clone())
                        .collect();
                    validate_effort(role, model, Some(&efforts), &path)?;
                    return Ok((model.provider.clone(), model.model.clone()));
                }
                return Err(invalid(format!(
                    "not listed in {}; refresh provider metadata or declare the exact ID/alias in {}",
                    path.display(),
                    configured.display()
                )));
            }
        }
        Err(invalid(format!(
            "no supported metadata entry; refresh provider metadata or declare the exact ID/alias in {}",
            configured.display()
        )))
    }

    pub fn validate_definition_models(&self, definition: &Definition) -> Result<()> {
        for (role, model) in ROLES.iter().zip(&definition.models) {
            if let Some(model) = model {
                self.validate_model(role, model)?;
            }
        }
        Ok(())
    }

    /// Compare provider identity, including explicitly declared aliases, without changing
    /// the identifier passed to the provider or stored in the revision.
    pub fn same_model(
        &self,
        role: &str,
        execution: &ModelConfig,
        judge: &ModelConfig,
    ) -> Result<bool> {
        Ok(self.model_identity("execution", execution)? == self.model_identity(role, judge)?)
    }

    pub fn validate_judge_model(&self, definition: &Definition, index: usize) -> Result<()> {
        if index > 0
            && let (Some(execution), Some(judge)) =
                (&definition.models[0], &definition.models[index])
        {
            let role = ROLES[index];
            require(
                !self.same_model(role, execution, judge)?,
                &format!(
                    "Configure a stronger {role} model distinct from the implementation model; changing effort or using an alias is insufficient"
                ),
            )?;
        }
        Ok(())
    }

    pub fn validate_judge_models(&self, definition: &Definition) -> Result<()> {
        self.validate_definition_models(definition)?;
        for index in 1..ROLES.len() {
            self.validate_judge_model(definition, index)?;
        }
        Ok(())
    }
}

fn validate_effort(
    role: &str,
    model: &ModelConfig,
    efforts: Option<&[String]>,
    source: &Path,
) -> Result<()> {
    let fallback = ModelConfig::accepted_efforts(&model.provider)
        .unwrap_or_default()
        .iter()
        .map(|effort| (*effort).to_owned())
        .collect::<Vec<_>>();
    let efforts = efforts.unwrap_or(&fallback);
    if let Some(effort) = &model.effort {
        require(
            efforts.contains(effort),
            &format!(
                "{role} effort {effort:?} is invalid\naccepted values: {}\nModel {}/{}; metadata {}",
                if efforts.is_empty() {
                    "none".into()
                } else {
                    efforts.join(", ")
                },
                model.provider,
                model.model,
                source.display(),
            ),
        )?;
    }
    Ok(())
}
