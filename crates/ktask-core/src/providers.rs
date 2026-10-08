//! Provider definitions and the pure catalogue view shared by the CLI and terminal UI.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

/// The output encodings the generic provider adapter understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderParser {
    /// Ordinary text, with configured markers searched in it.
    Plain,
    /// Claude Code's line-delimited structured event stream.
    ClaudeStreamJson,
    /// Codex's line-delimited structured event stream.
    CodexJsonl,
}

impl fmt::Display for ProviderParser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain => f.write_str("plain"),
            Self::ClaudeStreamJson => f.write_str("claude-stream-json"),
            Self::CodexJsonl => f.write_str("codex-jsonl"),
        }
    }
}

/// A complete provider definition. The argument lists are templates: `{prompt}`, `{model}`,
/// `{session}` and `{project-dir}` are replaced for an invocation. `denied-tools` is the list
/// passed to a provider's tool-denial option. The prompt is also supplied on standard input.
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
    /// Complete arguments used instead of the ordinary invocation when resuming. This supports
    /// CLIs, such as Codex, whose resume subcommand has a different command shape.
    #[serde(rename = "resume-command", default)]
    pub resume_command: Vec<String>,
    /// Tool names that must not be available during an unattended invocation.
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
    /// Short names this provider accepts in place of a model's full reported name — `sonnet`
    /// for `claude-sonnet-*`, say — each a literal prefix optionally ending `*` to match a
    /// whole family of dated releases. See [`model_matches`].
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
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
    /// Replacement complete resume invocation arguments.
    #[serde(rename = "resume-command")]
    pub resume_command: Option<Vec<String>>,
    /// Replacement tool deny list.
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
    /// Aliases added to, or replacing a built-in's own name for, the built-in's model
    /// aliases — a project adds one without repeating the ones it keeps.
    pub aliases: Option<BTreeMap<String, String>>,
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
        ("resume-command", definition.resume_command.join(" ")),
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
        ("aliases", format_aliases(&definition.aliases)),
    ]
}

/// `aliases` rendered as `name=pattern` pairs, in name order, for display.
fn format_aliases(aliases: &BTreeMap<String, String>) -> String {
    aliases
        .iter()
        .map(|(name, pattern)| format!("{name}={pattern}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a model named `requested` — a setting, or a resolver's own `retry --model` — names
/// the model `reported` — what the provider itself said it ran with. True when the two are
/// equal, or `requested` is an alias in `aliases` whose pattern matches `reported`: a literal
/// prefix, or, when it ends `*`, any reported name starting with the part before it — so a
/// short family name like `sonnet` still names a provider's own dated release. A `reported`
/// name outside every alias's pattern, asked for under that alias, is still a mismatch.
#[must_use]
pub fn model_matches(requested: &str, reported: &str, aliases: &BTreeMap<String, String>) -> bool {
    requested == reported
        || aliases
            .get(requested)
            .is_some_and(|pattern| matches_pattern(pattern, reported))
}

/// `text` matches `pattern`: equal, or, when `pattern` ends `*`, `text` starts with the part
/// before it.
fn matches_pattern(pattern: &str, text: &str) -> bool {
    pattern
        .strip_suffix('*')
        .map_or(pattern == text, |prefix| text.starts_with(prefix))
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
                resume_command: Vec::new(),
                denied_tools: Vec::new(),
                parser: ProviderParser::Plain,
                session_id: None,
                usage: None,
                limit_message: None,
                aliases: BTreeMap::new(),
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

/// Overlays `patch`'s argument-list and scalar fields on `definition`, each field named in
/// `changed` when `patch` set it at all.
fn apply(
    definition: &mut ProviderDefinition,
    patch: &ProviderOverride,
    changed: &mut BTreeSet<String>,
) {
    apply_argument_lists(definition, patch, changed);
    apply_scalar_fields(definition, patch, changed);
}

/// Overlays every field a provider's command line is built from — whole-list replacements,
/// never merged with the built-in's own.
fn apply_argument_lists(
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
    replace!(resume_command, "resume-command");
    replace!(denied_tools, "denied-tools");
}

/// Overlays every field that reads a provider's own output: the parser and each of its
/// single-value readers replace the built-in's own, while `aliases` — a name-to-pattern map —
/// adds to it instead.
fn apply_scalar_fields(
    definition: &mut ProviderDefinition,
    patch: &ProviderOverride,
    changed: &mut BTreeSet<String>,
) {
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
    if let Some(value) = &patch.aliases {
        definition.aliases.extend(value.clone());
        changed.insert("aliases".to_owned());
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
                resume_command: Vec::new(),
                denied_tools: Vec::new(),
                parser: ProviderParser::Plain,
                session_id: None,
                usage: None,
                limit_message: None,
                aliases: BTreeMap::new(),
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

    #[test]
    fn a_project_alias_adds_to_the_built_ins_own_rather_than_replacing_them() {
        let builtins = BTreeMap::from([(
            "claude".to_owned(),
            ProviderDefinition {
                command: "claude".to_owned(),
                args: Vec::new(),
                prompt: Vec::new(),
                model: Vec::new(),
                resume: Vec::new(),
                resume_command: Vec::new(),
                denied_tools: Vec::new(),
                parser: ProviderParser::Plain,
                session_id: None,
                usage: None,
                limit_message: None,
                aliases: BTreeMap::from([("haiku".to_owned(), "claude-haiku-*".to_owned())]),
            },
        )]);
        let project = ProviderOverride {
            aliases: Some(BTreeMap::from([(
                "fast".to_owned(),
                "claude-haiku-*".to_owned(),
            )])),
            ..ProviderOverride::default()
        };
        let views =
            provider_views(&builtins, &BTreeMap::from([("claude".to_owned(), project)])).unwrap();
        let claude = views.iter().find(|view| view.name == "claude").unwrap();
        assert_eq!(
            claude.definition.aliases,
            BTreeMap::from([
                ("fast".to_owned(), "claude-haiku-*".to_owned()),
                ("haiku".to_owned(), "claude-haiku-*".to_owned()),
            ])
        );
        assert!(claude.overridden.contains("aliases"));
    }

    #[test]
    fn model_matches_is_exact_or_an_aliased_family_prefix() {
        let aliases = BTreeMap::from([
            ("sonnet".to_owned(), "claude-sonnet-*".to_owned()),
            ("opus".to_owned(), "claude-opus-*".to_owned()),
            ("haiku".to_owned(), "claude-haiku-*".to_owned()),
        ]);
        assert!(model_matches(
            "claude-haiku-4-5-20251001",
            "claude-haiku-4-5-20251001",
            &aliases
        ));
        assert!(model_matches(
            "haiku",
            "claude-haiku-4-5-20251001",
            &aliases
        ));
        assert!(model_matches("sonnet", "claude-sonnet-5", &aliases));
        assert!(!model_matches(
            "sonnet",
            "claude-haiku-4-5-20251001",
            &aliases
        ));
        assert!(!model_matches("haiku", "claude-sonnet-5", &aliases));
        // An unaliased short name is not silently accepted.
        assert!(!model_matches(
            "unknown-alias",
            "claude-haiku-4-5",
            &aliases
        ));
    }
}
