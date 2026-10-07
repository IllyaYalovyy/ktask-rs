//! The immutable project facts every step of a queue run reads.

use std::path::Path;
use std::time::Duration;

/// Where a run executes: its project, binary, configured steps and agent models.
#[derive(Debug, Clone, Copy)]
pub struct RunContext<'a> {
    /// Project identity used in attempt tokens.
    pub project_name: &'a str,
    /// Directory providers and commands run in.
    pub project_dir: &'a Path,
    /// The binary agents call to report outcomes.
    pub binary_path: &'a Path,
    /// Per-attempt time limit.
    pub attempt_timeout: Duration,
    /// Optional health-check command.
    pub health_check_command: Option<&'a str>,
    /// Optional check command, run after implementation.
    pub check_command: Option<&'a str>,
    /// Optional remote branch to synchronize.
    pub tracked_branch: Option<&'a str>,
    /// Steps explicitly disabled by project settings.
    pub disabled_steps: &'a [&'static str],
    /// Maximum agent attempts before resolve stops retrying.
    pub max_attempts: u32,
    /// Maximum consecutive transport failures retried within one attempt.
    pub transport_retries: u32,
    /// Used by implementation, review and test steps; empty means no model was selected.
    pub model: &'a str,
    /// Used only by resolve; empty means no model was selected.
    pub resolver_model: &'a str,
    /// Tool-state directory for session transcripts.
    pub sessions_dir: &'a Path,
    /// Tool-state directory for appended provider output.
    pub outputs_dir: &'a Path,
}

impl RunContext<'_> {
    /// Where the retained output of step `step` of the attempt `token` names is kept.
    pub(crate) fn step_output_path(
        &self,
        token: &crate::AttemptToken,
        step: &str,
    ) -> std::path::PathBuf {
        self.outputs_dir
            .join(crate::step_output_file_name(token.task, token.number, step))
    }

    /// Whether the step named `step` is switched on.
    pub(crate) fn step_enabled(&self, step: &str) -> bool {
        !self.disabled_steps.contains(&step)
    }
}
