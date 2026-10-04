//! The test step: runs the provider in the tester role over the diff the implementation step
//! made, when switched on.

use std::fmt::Write as _;
use std::path::Path;

use crate::steps::{Deps, PipelineState, Step, StepOutcome, diff_since, run_agent_step};
use crate::{AttemptToken, RunContext, RunError, Task};

/// The journal name of the testing step.
pub const TEST_STEP: &str = "testing";

/// The prompt for the test step of attempt `token` of `task`: its title, body and acceptance
/// criteria, the diff the implementation step made — `diff`, empty when there was nothing to
/// compare against or git could not produce one — and the exact `report` command, run through
/// `binary_path`, to run for each possible outcome.
#[must_use]
pub fn build_test_prompt(
    task: &Task,
    token: &AttemptToken,
    binary_path: &Path,
    diff: &str,
) -> String {
    let mut prompt = format!("# Test: {}\n", task.title);
    if !task.body.is_empty() {
        prompt.push('\n');
        prompt.push_str(&task.body);
        prompt.push('\n');
    }
    prompt.push_str("\n## Acceptance criteria\n\n");
    for criterion in &task.criteria {
        prompt.push_str("- ");
        prompt.push_str(criterion);
        prompt.push('\n');
    }
    prompt.push_str("\n## What the task changed\n\n```diff\n");
    prompt.push_str(diff);
    if !diff.is_empty() && !diff.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push_str("```\n");
    let binary = binary_path.display();
    let _ = write!(
        prompt,
        "\n## Reporting\n\n\
         Try out the change above against the task and its acceptance criteria. When you are \
         done, run exactly one of these, with the outcome that fits:\n\n\
         \x20\x20\x20\x20{binary} report --token {token} accepted\n\
         \x20\x20\x20\x20{binary} report --token {token} rejected --reason \"<what failed>\"\n"
    );
    prompt
}

/// The test step of a task's attempt.
pub(crate) struct Test;

impl Step for Test {
    fn name(&self) -> &'static str {
        TEST_STEP
    }

    fn enabled(&self, context: RunContext<'_>, _state: &PipelineState<'_>) -> bool {
        context.step_enabled(TEST_STEP)
    }

    fn run(
        &self,
        deps: &Deps<'_>,
        context: RunContext<'_>,
        state: &mut PipelineState<'_>,
    ) -> Result<StepOutcome, RunError> {
        let diff = diff_since(deps.git, context, state.start_commit.as_deref());
        let prompt = build_test_prompt(state.task, state.token, context.binary_path, &diff);
        run_agent_step(deps, context, state, TEST_STEP, None, &prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::at;
    use crate::{TaskId, TaskKind, TaskStatus};

    #[test]
    fn build_test_prompt_carries_the_title_criteria_the_diff_and_the_exact_report_commands() {
        let task = Task {
            id: TaskId(7),
            position: 1,
            title: "Do the thing".to_owned(),
            body: "Some body text.".to_owned(),
            criteria: vec!["first thing".to_owned()],
            kind: TaskKind::Agent,
            links: vec![],
            status: TaskStatus::Running,
            created_at: at(1),
        };
        let token = AttemptToken::new("proj", TaskId(7), 3);
        let binary_path = Path::new("/opt/ktask-rs/bin/ktask-rs");
        let diff = "--- a/file\n+++ b/file\n+added line\n";
        let prompt = build_test_prompt(&task, &token, binary_path, diff);
        assert!(prompt.contains("Do the thing"), "{prompt}");
        assert!(prompt.contains("Some body text."), "{prompt}");
        assert!(prompt.contains("- first thing"), "{prompt}");
        assert!(prompt.contains("+added line"), "{prompt}");
        assert!(
            prompt.contains("/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 accepted"),
            "{prompt}"
        );
        assert!(
            prompt.contains("/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 rejected --reason"),
            "{prompt}"
        );
        assert!(!prompt.contains("\n    ktask-rs report"), "{prompt}");
    }

    #[test]
    fn disabled_when_switched_off() {
        let context = RunContext {
            project_name: "proj",
            project_dir: Path::new("/work/proj"),
            binary_path: Path::new("/opt/ktask-rs/bin/ktask-rs"),
            attempt_timeout: std::time::Duration::from_secs(60),
            health_check_command: None,
            tracked_branch: None,
            disabled_steps: &[TEST_STEP],
            max_attempts: 1,
            resolver_model: "",
            sessions_dir: Path::new("/state/sessions"),
            outputs_dir: Path::new("/state/outputs"),
        };
        let task = Task {
            id: TaskId(1),
            position: 1,
            title: "a".to_owned(),
            body: String::new(),
            criteria: vec![],
            kind: TaskKind::Agent,
            links: vec![],
            status: TaskStatus::Running,
            created_at: at(1),
        };
        let token = AttemptToken::new("proj", TaskId(1), 1);
        let state = PipelineState {
            task: &task,
            token: &token,
            start_commit: None,
            committed: None,
            exit_code: None,
            failure: None,
            requested_model: None,
            requested_session: None,
            known_cause: false,
            usage: crate::Usage::default(),
            used_model: None,
            limit_warning: None,
        };
        assert!(!Test.enabled(context, &state));
    }
}
