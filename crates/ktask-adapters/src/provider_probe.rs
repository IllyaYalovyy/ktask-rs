//! Real readiness probes for configured providers.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ktask_core::{
    Exit, ProbeCall, ProviderDefinition, ProviderProbe, ProviderRunError, ProviderView, StepCall,
    run_provider,
};

use crate::{ProcessCommands, configured_provider, echo};

/// The real process-backed implementation of the provider-readiness port.
#[derive(Debug)]
pub struct ProcessProviderProbe {
    dir: PathBuf,
}

impl ProcessProviderProbe {
    /// Makes readiness calls start in `dir`, as ordinary provider runs do.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

impl ProviderProbe for ProcessProviderProbe {
    fn always_ready(&self, provider: &ProviderView) -> bool {
        provider.name == echo::NAME
    }

    fn readiness_model(&self, provider: &ProviderView) -> Option<&str> {
        (provider.name == "claude").then_some("claude-haiku-4-5")
    }

    fn command_present(&self, command: &str) -> Result<bool, String> {
        Ok(command_paths(command).into_iter().any(is_executable))
    }

    fn smallest_call(
        &self,
        definition: &ProviderDefinition,
        model: Option<&str>,
    ) -> Result<ProbeCall, String> {
        let provider = configured_provider("readiness", definition);
        let output = run_provider(
            &ProcessCommands,
            &provider,
            "Reply with READY and do not use tools.",
            StepCall {
                token: "provider-check",
                attempt: 1,
                step: "readiness",
                model,
                resume: None,
                prompt_path: Path::new(""),
            },
            &self.dir,
            Duration::from_secs(15),
            None,
        )
        .map_err(probe_error)?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(ProbeCall {
            succeeded: output.exit == Exit::Code(0),
            authentication_failed: authentication_failure(&text),
            problem: (output.exit != Exit::Code(0)).then(|| compact_problem(&text, output.exit)),
        })
    }
}

/// Candidate executable paths for a command name or a path supplied directly.
fn command_paths(command: &str) -> Vec<PathBuf> {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return vec![path.to_path_buf()];
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(command))
                .collect()
        })
        .unwrap_or_default()
}

fn is_executable(path: PathBuf) -> bool {
    path.is_file()
        && std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

fn authentication_failure(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["not logged in", "invalid api key", "please run /login"]
        .iter()
        .any(|phrase| lower.contains(phrase))
}

fn compact_problem(text: &str, exit: Exit) -> String {
    let text = text.trim();
    if !text.is_empty() {
        return text.lines().next().unwrap_or_default().to_owned();
    }
    match exit {
        Exit::Code(code) => format!("the command exited {code}"),
        Exit::Killed => "the command timed out".to_owned(),
        Exit::Interrupted => "the command was interrupted".to_owned(),
    }
}

fn probe_error(error: ProviderRunError) -> String {
    match error {
        ProviderRunError::Build(message) => message,
        ProviderRunError::Commands(error) => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::authentication_failure;

    #[test]
    fn recognizes_recorded_claude_login_errors() {
        assert!(authentication_failure("Invalid API key; please run /login"));
        assert!(authentication_failure("Not logged in"));
        assert!(!authentication_failure("network unavailable"));
    }
}
