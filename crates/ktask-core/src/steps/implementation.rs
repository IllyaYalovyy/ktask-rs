//! The implementation step: hands the task's own prompt to the provider. Always runs — it is
//! the one step of the seven that cannot be switched off.

use crate::steps::{Deps, PipelineState, Step, StepOutcome, run_agent_step};
use crate::{AttemptToken, IMPLEMENTATION, RunContext, RunError, Task};

/// The prompt for attempt `token` of `task`: its title, body and acceptance criteria, and the
/// exact `report` command, run through `binary_path`, to run for each possible outcome. The
/// full path is used, rather than the name `ktask-rs`, so the command works whether or not the
/// binary that is running is on the agent's `PATH`.
#[must_use]
pub fn build_prompt(task: &Task, token: &AttemptToken, binary_path: &std::path::Path) -> String {
    use std::fmt::Write as _;

    let mut prompt = format!("# {}\n", task.title);
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
    let binary = binary_path.display();
    let _ = write!(
        prompt,
        "\n## Reporting\n\n\
         When you are done, run exactly one of these, with the outcome that fits:\n\n\
         \x20\x20\x20\x20{binary} report --token {token} done\n\
         \x20\x20\x20\x20{binary} report --token {token} failed --reason \"<why>\"\n\
         \x20\x20\x20\x20{binary} report --token {token} needs-input --reason \"<why>\"\n\
         \x20\x20\x20\x20{binary} report --token {token} too-large --reason \"<why>\"\n"
    );
    prompt
}

/// The implementation step of a task's attempt.
pub(crate) struct Implementation;

impl Step for Implementation {
    fn name(&self) -> &'static str {
        IMPLEMENTATION
    }

    fn enabled(&self, _context: RunContext<'_>, _state: &PipelineState<'_>) -> bool {
        true
    }

    fn run(
        &self,
        deps: &Deps<'_>,
        context: RunContext<'_>,
        state: &mut PipelineState<'_>,
    ) -> Result<StepOutcome, RunError> {
        let prompt = build_prompt(state.task, state.token, context.binary_path);
        run_agent_step(deps, context, state, IMPLEMENTATION, &prompt)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::fakes::at;
    use crate::{TaskId, TaskKind, TaskStatus};

    #[test]
    fn build_prompt_carries_the_title_body_criteria_and_the_exact_report_command() {
        let task = Task {
            id: TaskId(7),
            position: 1,
            title: "Do the thing".to_owned(),
            body: "Some body text.".to_owned(),
            criteria: vec!["first thing".to_owned(), "second thing".to_owned()],
            kind: TaskKind::Agent,
            links: vec![],
            status: TaskStatus::Running,
            created_at: at(1),
        };
        let token = AttemptToken::new("proj", TaskId(7), 3);
        let binary_path = Path::new("/opt/ktask-rs/bin/ktask-rs");
        let prompt = build_prompt(&task, &token, binary_path);
        assert!(prompt.contains("Do the thing"), "{prompt}");
        assert!(prompt.contains("Some body text."), "{prompt}");
        assert!(prompt.contains("- first thing"), "{prompt}");
        assert!(prompt.contains("- second thing"), "{prompt}");
        assert!(
            prompt.contains("/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 done"),
            "{prompt}"
        );
        assert!(
            prompt.contains("/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 failed --reason"),
            "{prompt}"
        );
        assert!(
            prompt.contains(
                "/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 needs-input --reason"
            ),
            "{prompt}"
        );
        assert!(
            prompt
                .contains("/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 too-large --reason"),
            "{prompt}"
        );
        // No line names the binary by its bare name alone: an agent that runs the shown
        // command must never depend on `ktask-rs` being on its own `PATH`.
        assert!(!prompt.contains("\n    ktask-rs report"), "{prompt}");
    }

    #[test]
    fn implementation_is_never_switched_off() {
        let context = RunContext {
            project_name: "proj",
            project_dir: Path::new("/work/proj"),
            binary_path: Path::new("/opt/ktask-rs/bin/ktask-rs"),
            attempt_timeout: std::time::Duration::from_secs(60),
            health_check_command: None,
            tracked_branch: None,
            disabled_steps: &[IMPLEMENTATION],
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
        };
        assert!(Implementation.enabled(context, &state));
        assert_eq!(Implementation.name(), IMPLEMENTATION);
    }
}
