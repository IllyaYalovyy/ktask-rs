//! Provider definitions and the pure catalogue view shared by the CLI and terminal UI.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

/// The two output encodings the generic provider adapter understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderParser {
    /// Ordinary text, with configured markers searched in it.
    Plain,
    /// Claude Code's line-delimited structured event stream.
    ClaudeStreamJson,
}

impl fmt::Display for ProviderParser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain => f.write_str("plain"),
            Self::ClaudeStreamJson => f.write_str("claude-stream-json"),
        }
    }
}

/// A complete provider definition. The argument lists are templates: `{prompt}`, `{model}`
/// and `{session}` are replaced for an invocation; `{denied-tools}` becomes a comma-separated
/// list. The prompt is also supplied on standard input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDefinition {
    /// Program found on `PATH`, or an absolute program path.
    pub command: String,
    /// Arguments supplied for every invocation.
    #[serde(default)]
    pub args: Vec<String>,
    /// Arguments that carry the prompt when this CLI needs it as an argument.
    #[serde(default)]
    pub prompt: Vec<String>,
    /// Arguments appended when a model is selected.
    #[serde(default)]
    pub model: Vec<String>,
    /// Arguments appended when an earlier session is resumed.
    #[serde(default)]
    pub resume: Vec<String>,
    /// Arguments that convey the tool deny list.
    #[serde(rename = "denied-tools", default)]
    pub denied_tools: Vec<String>,
    /// The output encoding to parse.
    pub parser: ProviderParser,
    /// The text or event field that carries a session identifier.
    #[serde(rename = "session-id", default)]
    pub session_id: Option<String>,
    /// The text or event field that carries usage data.
    #[serde(default)]
    pub usage: Option<String>,
    /// A regular expression that means the provider limit was reached. A `reset` capture,
    /// when present, is Unix seconds at which the provider says the limit resets.
    #[serde(rename = "limit-message", default)]
    pub limit_message: Option<String>,
}

/// The fields a project may replace on a built-in definition. A new provider must set at
/// least `command`; omitted fields take their ordinary empty/default value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderOverride {
    /// Replacement program.
    pub command: Option<String>,
    /// Replacement common arguments.
    pub args: Option<Vec<String>>,
    /// Replacement prompt arguments.
    pub prompt: Option<Vec<String>>,
    /// Replacement model arguments.
    pub model: Option<Vec<String>>,
    /// Replacement resume arguments.
    pub resume: Option<Vec<String>>,
    /// Replacement denied-tools arguments.
    #[serde(rename = "denied-tools")]
    pub denied_tools: Option<Vec<String>>,
    /// Replacement output parser.
    pub parser: Option<ProviderParser>,
    /// Replacement session-id reader.
    #[serde(rename = "session-id")]
    pub session_id: Option<String>,
    /// Replacement usage reader.
    pub usage: Option<String>,
    /// Replacement limit-message reader.
    #[serde(rename = "limit-message")]
    pub limit_message: Option<String>,
}

/// A provider ready to render, including the project fields that replaced built-in values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderView {
    /// The provider's configuration name.
    pub name: String,
    /// The fully overlaid definition.
    pub definition: ProviderDefinition,
    /// Field names supplied by the project rather than the built-in.
    pub overridden: BTreeSet<String>,
    /// Whether this name began as a shipped definition.
    pub built_in: bool,
}

/// Where one rendered field came from: the project, a shipped definition, or a new provider's
/// empty default.
#[must_use]
pub fn provider_field_source(provider: &ProviderView, field: &str) -> &'static str {
    if provider.overridden.contains(field) {
        "project"
    } else if provider.built_in {
        "built-in"
    } else {
        "default"
    }
}

/// The complete definition in the stable display order used by both frontends.
#[must_use]
pub fn provider_fields(provider: &ProviderView) -> Vec<(&'static str, String)> {
    let definition = &provider.definition;
    vec![
        ("command", definition.command.clone()),
        ("args", definition.args.join(" ")),
        ("prompt", definition.prompt.join(" ")),
        ("model", definition.model.join(" ")),
        ("resume", definition.resume.join(" ")),
        ("denied-tools", definition.denied_tools.join(" ")),
        ("parser", definition.parser.to_string()),
        (
            "session-id",
            definition.session_id.clone().unwrap_or_default(),
        ),
        ("usage", definition.usage.clone().unwrap_or_default()),
        (
            "limit-message",
            definition.limit_message.clone().unwrap_or_default(),
        ),
    ]
}

/// Validates and overlays `overrides` on `builtins`. The error always names the provider
/// field that needs correcting, so a TOML settings reader can report it directly.
///
/// # Errors
///
/// Returns an error when a new definition omits its command.
pub fn provider_views(
    builtins: &BTreeMap<String, ProviderDefinition>,
    overrides: &BTreeMap<String, ProviderOverride>,
) -> Result<Vec<ProviderView>, String> {
    let (definitions, changed) = overlay_definitions(builtins, overrides);
    definitions
        .into_iter()
        .map(|(name, definition)| provider_view(builtins, &changed, name, definition))
        .collect()
}

/// Applies every project-supplied patch to a fresh built-in catalogue.
fn overlay_definitions(
    builtins: &BTreeMap<String, ProviderDefinition>,
    overrides: &BTreeMap<String, ProviderOverride>,
) -> (
    BTreeMap<String, ProviderDefinition>,
    BTreeMap<String, BTreeSet<String>>,
) {
    let mut definitions = builtins.clone();
    let mut changed = BTreeMap::<String, BTreeSet<String>>::new();
    for (name, patch) in overrides {
        let built_in = definitions.contains_key(name);
        let definition = definitions
            .entry(name.clone())
            .or_insert_with(|| ProviderDefinition {
                command: String::new(),
                args: Vec::new(),
                prompt: Vec::new(),
                model: Vec::new(),
                resume: Vec::new(),
                denied_tools: Vec::new(),
                parser: ProviderParser::Plain,
                session_id: None,
                usage: None,
                limit_message: None,
            });
        let fields = changed.entry(name.clone()).or_default();
        if !built_in {
            fields.insert("command".to_owned());
        }
        apply(definition, patch, fields);
    }
    (definitions, changed)
}

/// Makes one effective definition renderable, rejecting a new provider with no command.
fn provider_view(
    builtins: &BTreeMap<String, ProviderDefinition>,
    changed: &BTreeMap<String, BTreeSet<String>>,
    name: String,
    definition: ProviderDefinition,
) -> Result<ProviderView, String> {
    if definition.command.trim().is_empty() {
        return Err(format!("providers.{name}.command: must not be empty"));
    }
    Ok(ProviderView {
        overridden: changed.get(&name).cloned().unwrap_or_default(),
        built_in: builtins.contains_key(&name),
        name,
        definition,
    })
}

fn apply(
    definition: &mut ProviderDefinition,
    patch: &ProviderOverride,
    changed: &mut BTreeSet<String>,
) {
    macro_rules! replace {
        ($field:ident, $name:literal) => {
            if let Some(value) = &patch.$field {
                definition.$field = value.clone();
                changed.insert($name.to_owned());
            }
        };
    }
    replace!(command, "command");
    replace!(args, "args");
    replace!(prompt, "prompt");
    replace!(model, "model");
    replace!(resume, "resume");
    replace!(denied_tools, "denied-tools");
    if let Some(value) = patch.parser {
        definition.parser = value;
        changed.insert("parser".to_owned());
    }
    if let Some(value) = &patch.session_id {
        definition.session_id = Some(value.clone());
        changed.insert("session-id".to_owned());
    }
    if let Some(value) = &patch.usage {
        definition.usage = Some(value.clone());
        changed.insert("usage".to_owned());
    }
    if let Some(value) = &patch.limit_message {
        definition.limit_message = Some(value.clone());
        changed.insert("limit-message".to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_definitions_overlay_builtins_and_must_name_a_command_when_new() {
        let custom = ProviderOverride {
            command: Some("agent".to_owned()),
            ..ProviderOverride::default()
        };
        let claude = ProviderOverride {
            command: Some("local-claude".to_owned()),
            ..ProviderOverride::default()
        };
        let builtins = BTreeMap::from([(
            "built-in".to_owned(),
            ProviderDefinition {
                command: "agent".to_owned(),
                args: Vec::new(),
                prompt: Vec::new(),
                model: Vec::new(),
                resume: Vec::new(),
                denied_tools: Vec::new(),
                parser: ProviderParser::Plain,
                session_id: None,
                usage: None,
                limit_message: None,
            },
        )]);
        let views = provider_views(
            &builtins,
            &BTreeMap::from([
                ("custom".to_owned(), custom),
                ("built-in".to_owned(), claude),
            ]),
        )
        .unwrap();
        assert_eq!(
            views
                .iter()
                .map(|view| view.name.as_str())
                .collect::<Vec<_>>(),
            vec!["built-in", "custom"]
        );
        let claude = views.iter().find(|view| view.name == "built-in").unwrap();
        assert_eq!(claude.definition.command, "local-claude");
        assert!(claude.overridden.contains("command"));
        assert!(claude.built_in);
        assert_eq!(
            provider_views(
                &builtins,
                &BTreeMap::from([("broken".to_owned(), ProviderOverride::default())])
            ),
            Err("providers.broken.command: must not be empty".to_owned())
        );
    }
}
