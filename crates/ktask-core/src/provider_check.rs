//! Provider-readiness facts and the use case that assembles them.

use crate::{ProviderDefinition, ProviderView};

/// The three things an operator needs to know before trusting a provider with a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderCheckKind {
    /// The configured executable can be found.
    Command,
    /// The provider accepted credentials for a request.
    Login,
    /// A deliberately small provider request completed.
    Call,
}

/// One readiness result, including the next action when it did not pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCheckItem {
    /// What was checked.
    pub kind: ProviderCheckKind,
    /// Whether it passed.
    pub passed: bool,
    /// The concrete action that fixes a failed check.
    pub advice: Option<String>,
}

/// The complete readiness result for one effective provider definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCheck {
    /// The checked provider's configured name.
    pub provider: String,
    /// Results in the order command, login, smallest call.
    pub items: Vec<ProviderCheckItem>,
}

impl ProviderCheck {
    /// Whether every readiness check passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.items.iter().all(|item| item.passed)
    }
}

/// The outside-world operations a readiness check needs.
pub trait ProviderProbe {
    /// Whether this provider needs no external probe call to establish readiness.
    fn always_ready(&self, provider: &ProviderView) -> bool;

    /// The low-cost model to use for this provider's readiness request, when it has one.
    fn readiness_model(&self, provider: &ProviderView) -> Option<&str>;

    /// Whether `command` resolves to an executable in this process's `PATH`.
    ///
    /// # Errors
    ///
    /// Returns an error when the host cannot inspect its command search path.
    fn command_present(&self, command: &str) -> Result<bool, String>;

    /// Makes the smallest useful call through `definition`, using `model` when one is named.
    ///
    /// # Errors
    ///
    /// Returns an error when the probe cannot be started or observed at all.
    fn smallest_call(
        &self,
        definition: &ProviderDefinition,
        model: Option<&str>,
    ) -> Result<ProbeCall, String>;
}

/// How the probe call itself ended, normalized for the readiness use case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeCall {
    /// Whether the provider process exited successfully.
    pub succeeded: bool,
    /// Whether its output says that credentials are missing or expired.
    pub authentication_failed: bool,
    /// A concise provider error suitable for an operator-facing result.
    pub problem: Option<String>,
}

/// Looks up `name` and checks the resulting effective definition through `probe`.
///
/// A probe can declare an entirely local provider ready immediately; otherwise its selected
/// low-cost model, if any, is used for the one request that proves both authentication and a
/// real call.
///
/// # Errors
///
/// Returns an error when `name` is not configured or the probe's host operation fails.
pub fn check_provider(
    providers: &[ProviderView],
    name: &str,
    probe: &impl ProviderProbe,
) -> Result<ProviderCheck, String> {
    let provider = providers
        .iter()
        .find(|provider| provider.name == name)
        .ok_or_else(|| format!("unknown provider {name:?}"))?;
    if probe.always_ready(provider) {
        return Ok(passed_check(&provider.name));
    }
    let present = probe.command_present(&provider.definition.command)?;
    if !present {
        return Ok(missing_command(provider));
    }
    let call = probe.smallest_call(&provider.definition, probe.readiness_model(provider))?;
    Ok(called_provider(provider, call))
}

fn passed_check(name: &str) -> ProviderCheck {
    ProviderCheck {
        provider: name.to_owned(),
        items: vec![
            passed(ProviderCheckKind::Command),
            passed(ProviderCheckKind::Login),
            passed(ProviderCheckKind::Call),
        ],
    }
}

fn passed(kind: ProviderCheckKind) -> ProviderCheckItem {
    ProviderCheckItem {
        kind,
        passed: true,
        advice: None,
    }
}

fn missing_command(provider: &ProviderView) -> ProviderCheck {
    let command = &provider.definition.command;
    let advice = match provider.name.as_str() {
        "claude" => {
            "the `claude` binary is not on PATH; install Claude Code with `npm install -g @anthropic-ai/claude-code`".to_owned()
        }
        "codex" => {
            "the `codex` binary is not on PATH; install Codex with `npm install -g @openai/codex`"
                .to_owned()
        }
        _ => format!("install `{command}` and make sure it is on PATH"),
    };
    ProviderCheck {
        provider: provider.name.clone(),
        items: vec![
            ProviderCheckItem {
                kind: ProviderCheckKind::Command,
                passed: false,
                advice: Some(advice),
            },
            ProviderCheckItem {
                kind: ProviderCheckKind::Login,
                passed: false,
                advice: Some("install the command first, then check again".to_owned()),
            },
            ProviderCheckItem {
                kind: ProviderCheckKind::Call,
                passed: false,
                advice: Some("install the command first, then check again".to_owned()),
            },
        ],
    }
}

fn called_provider(provider: &ProviderView, call: ProbeCall) -> ProviderCheck {
    if call.succeeded {
        return passed_check(&provider.name);
    }
    let problem = call
        .problem
        .unwrap_or_else(|| "the command failed".to_owned());
    let login_advice = match provider.name.as_str() {
        "claude" => "run `claude /login`, then check again".to_owned(),
        "codex" => "run `codex login`, then check again".to_owned(),
        _ => format!(
            "sign in to `{}`, then check again",
            provider.definition.command
        ),
    };
    let call_advice = format!("fix the provider error ({problem}), then check again");
    ProviderCheck {
        provider: provider.name.clone(),
        items: vec![
            passed(ProviderCheckKind::Command),
            ProviderCheckItem {
                kind: ProviderCheckKind::Login,
                passed: !call.authentication_failed,
                advice: call.authentication_failed.then_some(login_advice),
            },
            ProviderCheckItem {
                kind: ProviderCheckKind::Call,
                passed: false,
                advice: Some(call_advice),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::{ProviderParser, ProviderView};

    #[derive(Debug)]
    struct Probe {
        present: bool,
        call: ProbeCall,
    }

    impl ProviderProbe for Probe {
        fn always_ready(&self, _: &ProviderView) -> bool {
            false
        }

        fn readiness_model(&self, _: &ProviderView) -> Option<&str> {
            None
        }

        fn command_present(&self, _: &str) -> Result<bool, String> {
            Ok(self.present)
        }

        fn smallest_call(
            &self,
            _: &ProviderDefinition,
            _: Option<&str>,
        ) -> Result<ProbeCall, String> {
            Ok(self.call.clone())
        }
    }

    fn provider(name: &str) -> ProviderView {
        ProviderView {
            name: name.to_owned(),
            definition: ProviderDefinition {
                command: name.to_owned(),
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
            },
            overridden: BTreeSet::new(),
            built_in: true,
        }
    }

    #[test]
    fn missing_command_has_actionable_results_for_every_check() {
        let report = check_provider(
            &[provider("claude")],
            "claude",
            &Probe {
                present: false,
                call: ProbeCall {
                    succeeded: false,
                    authentication_failed: false,
                    problem: None,
                },
            },
        )
        .unwrap();
        assert!(!report.passed());
        assert!(report.items.iter().all(|item| item.advice.is_some()));
        assert!(
            report.items[0]
                .advice
                .as_deref()
                .unwrap()
                .contains("@anthropic-ai/claude-code")
        );
    }

    #[test]
    fn missing_codex_command_names_its_install_command() {
        let report = check_provider(
            &[provider("codex")],
            "codex",
            &Probe {
                present: false,
                call: ProbeCall {
                    succeeded: false,
                    authentication_failed: false,
                    problem: None,
                },
            },
        )
        .unwrap();
        assert_eq!(
            report.items[0].advice.as_deref(),
            Some(
                "the `codex` binary is not on PATH; install Codex with `npm install -g @openai/codex`"
            )
        );
    }

    #[test]
    fn failed_codex_login_says_how_to_sign_in() {
        let report = check_provider(
            &[provider("codex")],
            "codex",
            &Probe {
                present: true,
                call: ProbeCall {
                    succeeded: false,
                    authentication_failed: true,
                    problem: Some("authentication failed".to_owned()),
                },
            },
        )
        .unwrap();
        assert_eq!(
            report.items[1].advice.as_deref(),
            Some("run `codex login`, then check again")
        );
    }
}
