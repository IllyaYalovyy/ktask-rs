//! Built-in provider configuration. Core deliberately knows only generic definitions.

use std::collections::BTreeMap;

use ktask_core::{ProviderDefinition, ProviderParser};

/// The provider definitions shipped by this binary.
#[must_use]
pub fn builtin_providers() -> BTreeMap<String, ProviderDefinition> {
    BTreeMap::from([
        (
            "claude".to_owned(),
            ProviderDefinition {
                command: "claude".to_owned(),
                args: vec![
                    "--print".to_owned(),
                    "--output-format".to_owned(),
                    "stream-json".to_owned(),
                    "--verbose".to_owned(),
                    "--permission-mode".to_owned(),
                    "bypassPermissions".to_owned(),
                ],
                prompt: Vec::new(),
                model: vec!["--model".to_owned(), "{model}".to_owned()],
                resume: vec!["--resume".to_owned(), "{session}".to_owned()],
                denied_tools: vec!["--disallowedTools".to_owned(), "{denied-tools}".to_owned()],
                parser: ProviderParser::ClaudeStreamJson,
                session_id: Some("result.session_id".to_owned()),
                usage: Some("result.usage".to_owned()),
                limit_message: Some("rate limit".to_owned()),
            },
        ),
        (
            "echo".to_owned(),
            ProviderDefinition {
                command: "bash".to_owned(),
                args: vec![
                    "-s".to_owned(),
                    "{token}".to_owned(),
                    "{attempt}".to_owned(),
                    "{step}".to_owned(),
                ],
                prompt: Vec::new(),
                model: Vec::new(),
                resume: Vec::new(),
                denied_tools: Vec::new(),
                parser: ProviderParser::Plain,
                session_id: Some("KTASK_SESSION: ".to_owned()),
                usage: None,
                limit_message: Some("KTASK_LIMIT: ".to_owned()),
            },
        ),
    ])
}
