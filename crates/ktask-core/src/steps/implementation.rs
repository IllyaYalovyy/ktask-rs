//! The implementation step: hands the task's own prompt to the provider. Always runs — it is
//! the one step of the seven that cannot be switched off.

use crate::steps::{Deps, PipelineState, Step, StepOutcome, diff_since, run_agent_step};
use crate::{
    AttemptToken, IMPLEMENTATION, Journal, JournalError, RunContext, RunError, Task, TaskId,
};

/// One earlier attempt at the task this prompt is for, as [`earlier_attempts`] reads it back:
/// its number, and what it ended at in the agent's own words when it reported one, or the
/// tool's when it never did.
struct EarlierAttempt {
    number: u32,
    outcome: String,
    reason: Option<String>,
}

/// Every attempt at task `id` numbered before `before`, oldest first, with what each one
/// ended at and why — the agent's own reported outcome when it reported one, the tool's own
/// word for how it ended (`failed`, `blocked`, `failed-unknown`) otherwise, `needs-input`'s own
/// reason carrying the answer too, once [`crate::answer_task`] has recorded one. Empty when
/// `before` is the task's first attempt.
///
/// # Errors
///
/// Fails when the journal cannot be read.
fn earlier_attempts(
    journal: &dyn Journal,
    id: TaskId,
    before: u32,
) -> Result<Vec<EarlierAttempt>, JournalError> {
    crate::attempt::all_attempts(journal, id)?
        .into_iter()
        .filter(|attempt| attempt.number < before)
        .map(|attempt| {
            let reported = crate::attempt::last_report(journal, id, attempt.number)?;
            let (outcome, reason) = if let Some((outcome, reason)) = reported {
                (outcome.as_str().to_owned(), reason)
            } else {
                let end = attempt.ended;
                (
                    end.as_ref()
                        .map_or("failed-unknown", |end| end.status.as_str())
                        .to_owned(),
                    end.and_then(|end| end.reason),
                )
            };
            let reason = if outcome == "needs-input" {
                let answer = crate::attempt::answer_of(journal, id, attempt.number)?;
                crate::attempt::with_answer(reason, answer.as_deref())
            } else {
                reason
            };
            Ok(EarlierAttempt {
                number: attempt.number,
                outcome,
                reason,
            })
        })
        .collect()
}

/// Everything the task numbered `id` has changed since its very first attempt began,
/// committed or still sitting uncommitted — not merely since the current attempt's own start,
/// which would show nothing when a task is retried, since nothing is reverted between
/// attempts. Empty when it has no earlier attempt, or git could not produce it.
fn diff_since_first_attempt(
    journal: &dyn Journal,
    git: &dyn crate::Git,
    context: RunContext<'_>,
    id: TaskId,
) -> Result<String, JournalError> {
    let first_start_commit = crate::attempt::all_attempts(journal, id)?
        .into_iter()
        .next()
        .and_then(|attempt| attempt.start_commit);
    Ok(diff_since(git, context, first_start_commit.as_deref()))
}

/// Appends one `- attempt N: outcome — reason` line per entry of `earlier` to `prompt`, under
/// its own heading, then `diff` — everything the task has changed since its first attempt
/// began — under its own, fenced as a diff. Does nothing when `earlier` is empty: the task's
/// first attempt names no earlier one and shows no diff.
fn append_history(prompt: &mut String, earlier: &[EarlierAttempt], diff: &str) {
    use std::fmt::Write as _;

    if earlier.is_empty() {
        return;
    }
    prompt.push_str("\n## Earlier attempts\n\n");
    for attempt in earlier {
        match &attempt.reason {
            Some(reason) => {
                let _ = writeln!(
                    prompt,
                    "- attempt {}: {} — {reason}",
                    attempt.number, attempt.outcome
                );
            }
            None => {
                let _ = writeln!(prompt, "- attempt {}: {}", attempt.number, attempt.outcome);
            }
        }
    }
    prompt.push_str("\n## What the task has changed so far\n\n```diff\n");
    prompt.push_str(diff);
    if !diff.is_empty() && !diff.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push_str("```\n");
}

/// The prompt for attempt `token` of `task`: its title, body and acceptance criteria; when
/// `earlier` is not empty — this attempt is not the task's first, because it was retried after
/// an earlier one ended `failed`, `failed-unknown` or `blocked` — each earlier attempt's own
/// outcome and reason, and `diff`, everything the task has changed since its first attempt
/// began; and the exact `report` command, run through `binary_path`, to run for each possible
/// outcome. The full path is used, rather than the name `ktask-rs`, so the command works
/// whether or not the binary that is running is on the agent's `PATH`.
#[must_use]
fn build_prompt_with_history(
    task: &Task,
    token: &AttemptToken,
    binary_path: &std::path::Path,
    earlier: &[EarlierAttempt],
    diff: &str,
) -> String {
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
    append_history(&mut prompt, earlier, diff);
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

/// The prompt for attempt `token` of `task`, with no earlier attempt: its title, body and
/// acceptance criteria, and the exact `report` command, run through `binary_path`, to run for
/// each possible outcome.
#[must_use]
pub fn build_prompt(task: &Task, token: &AttemptToken, binary_path: &std::path::Path) -> String {
    build_prompt_with_history(task, token, binary_path, &[], "")
}

/// The exact prompt the implementation step would hand its provider right now for attempt
/// `token` of `task`: read fresh from `journal` and `git`, not from a run in progress — for a
/// caller, such as a test, that wants to predict what a real agent (one that is handed the
/// whole prompt, unlike the `echo` provider, which only runs its first fenced `bash` block)
/// would be shown for an attempt that has not started yet.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub fn implementation_prompt(
    journal: &impl Journal,
    git: &impl crate::Git,
    context: RunContext<'_>,
    task: &Task,
    token: &AttemptToken,
) -> Result<String, JournalError> {
    let earlier = earlier_attempts(journal, task.id, token.number)?;
    let diff = if earlier.is_empty() {
        String::new()
    } else {
        diff_since_first_attempt(journal, git, context, task.id)?
    };
    Ok(build_prompt_with_history(
        task,
        token,
        context.binary_path,
        &earlier,
        &diff,
    ))
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
        let earlier = earlier_attempts(deps.journal, state.task.id, state.token.number)?;
        let diff = if earlier.is_empty() {
            String::new()
        } else {
            diff_since_first_attempt(deps.journal, deps.git, context, state.task.id)?
        };
        let prompt = build_prompt_with_history(
            state.task,
            state.token,
            context.binary_path,
            &earlier,
            &diff,
        );
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
    fn a_first_attempts_prompt_names_no_earlier_attempt_or_diff() {
        let task = Task {
            id: TaskId(7),
            position: 1,
            title: "Do the thing".to_owned(),
            body: String::new(),
            criteria: vec!["it works".to_owned()],
            kind: TaskKind::Agent,
            links: vec![],
            status: TaskStatus::Running,
            created_at: at(1),
        };
        let token = AttemptToken::new("proj", TaskId(7), 1);
        let binary_path = Path::new("/opt/ktask-rs/bin/ktask-rs");
        let prompt = build_prompt(&task, &token, binary_path);
        assert!(!prompt.contains("Earlier attempts"), "{prompt}");
        assert!(!prompt.contains("What the task has changed"), "{prompt}");
    }

    #[test]
    fn a_retried_attempts_prompt_names_each_earlier_outcome_and_reason_and_the_diff() {
        let task = Task {
            id: TaskId(7),
            position: 1,
            title: "Do the thing".to_owned(),
            body: String::new(),
            criteria: vec!["it works".to_owned()],
            kind: TaskKind::Agent,
            links: vec![],
            status: TaskStatus::Running,
            created_at: at(1),
        };
        let token = AttemptToken::new("proj", TaskId(7), 3);
        let binary_path = Path::new("/opt/ktask-rs/bin/ktask-rs");
        let earlier = vec![
            EarlierAttempt {
                number: 1,
                outcome: "failed".to_owned(),
                reason: Some("it broke".to_owned()),
            },
            EarlierAttempt {
                number: 2,
                outcome: "needs-input".to_owned(),
                reason: Some("which path?".to_owned()),
            },
        ];
        let diff = "--- a/file\n+++ b/file\n+added line\n";
        let prompt = build_prompt_with_history(&task, &token, binary_path, &earlier, diff);
        assert!(
            prompt.contains("- attempt 1: failed — it broke"),
            "{prompt}"
        );
        assert!(
            prompt.contains("- attempt 2: needs-input — which path?"),
            "{prompt}"
        );
        assert!(prompt.contains("+added line"), "{prompt}");
        assert!(
            prompt.contains("/opt/ktask-rs/bin/ktask-rs report --token proj/7/3 done"),
            "{prompt}"
        );
    }

    #[test]
    fn earlier_attempts_reads_back_each_ones_reported_outcome_or_the_tools_own() {
        use crate::fakes::{FakeClock, FakeJournal, draft};
        use crate::{AttemptRun, Outcome, Placement, add_task, report};

        let journal = FakeJournal::default();
        let clock = FakeClock(at(0));
        add_task(&journal, &clock, &draft("a"), Placement::End).unwrap();

        // Attempt 1 is reported `failed`, with a reason.
        let token1 = crate::start_attempt(&journal, &clock, "proj", TaskId(1)).unwrap();
        report(&journal, &clock, &token1, Outcome::Failed, Some("it broke")).unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            1,
            AttemptRun {
                duration: std::time::Duration::ZERO,
                exit_code: Some(0),
                status: TaskStatus::Failed,
                reason: Some("it broke"),
            },
            clock.0,
        )
        .unwrap();
        crate::retry_task(&journal, &clock, TaskId(1)).unwrap();

        // Attempt 2 crashes and reports nothing: the tool's own word is read back instead.
        crate::attempt::begin_attempt_running(&journal, &clock, TaskId(1), "echo", None).unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            2,
            AttemptRun {
                duration: std::time::Duration::ZERO,
                exit_code: None,
                status: TaskStatus::FailedUnknown,
                reason: Some("the provider ran past its time limit and was killed"),
            },
            clock.0,
        )
        .unwrap();
        crate::retry_task(&journal, &clock, TaskId(1)).unwrap();

        let earlier = earlier_attempts(&journal, TaskId(1), 3).unwrap();
        assert_eq!(earlier.len(), 2);
        assert_eq!(earlier[0].number, 1);
        assert_eq!(earlier[0].outcome, "failed");
        assert_eq!(earlier[0].reason.as_deref(), Some("it broke"));
        assert_eq!(earlier[1].number, 2);
        assert_eq!(earlier[1].outcome, "failed-unknown");
        assert_eq!(
            earlier[1].reason.as_deref(),
            Some("the provider ran past its time limit and was killed")
        );
    }

    #[test]
    fn an_answered_attempts_earlier_line_and_so_the_next_prompt_carry_the_question_and_the_answer()
    {
        use crate::fakes::{FakeClock, FakeJournal, draft};
        use crate::{AttemptRun, Outcome, Placement, add_task, answer_task, report};

        let journal = FakeJournal::default();
        let clock = FakeClock(at(0));
        add_task(&journal, &clock, &draft("a"), Placement::End).unwrap();

        let token = crate::start_attempt(&journal, &clock, "proj", TaskId(1)).unwrap();
        report(
            &journal,
            &clock,
            &token,
            Outcome::NeedsInput,
            Some("which path?"),
        )
        .unwrap();
        crate::attempt::end_attempt(
            &journal,
            TaskId(1),
            1,
            AttemptRun {
                duration: std::time::Duration::ZERO,
                exit_code: Some(0),
                status: TaskStatus::Blocked,
                reason: Some("which path?"),
            },
            clock.0,
        )
        .unwrap();
        answer_task(&journal, &clock, TaskId(1), "the left one").unwrap();

        let earlier = earlier_attempts(&journal, TaskId(1), 2).unwrap();
        assert_eq!(
            earlier[0].reason.as_deref(),
            Some("which path? — answer: the left one")
        );

        let task = Task {
            id: TaskId(1),
            position: 1,
            title: "a".to_owned(),
            body: String::new(),
            criteria: vec!["it works".to_owned()],
            kind: TaskKind::Agent,
            links: vec![],
            status: TaskStatus::Pending,
            created_at: at(0),
        };
        let next_token = AttemptToken::new("proj", TaskId(1), 2);
        let binary_path = Path::new("/opt/ktask-rs/bin/ktask-rs");
        let earlier = vec![EarlierAttempt {
            number: 1,
            outcome: earlier[0].outcome.clone(),
            reason: earlier[0].reason.clone(),
        }];
        let prompt = build_prompt_with_history(&task, &next_token, binary_path, &earlier, "");
        assert!(prompt.contains("which path?"), "{prompt}");
        assert!(prompt.contains("the left one"), "{prompt}");
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
