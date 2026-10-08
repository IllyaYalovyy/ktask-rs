//! The single presentation for one task's full detail: every fact [`ktask_core::TaskDetail`]
//! carries, nothing elided — shared by the detail screen and `ktask-rs show`.

use ktask_core::{AttemptLine, StepLine, TaskDetail};

use super::{
    done_mark_text, limit_wait_text, limit_warning_text, more_time_text, outcome, reason,
    routed_text, session_suffix, step_usage_text, task_status,
};

/// One line of a task's detail, in the order [`detail_lines`] builds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailLine {
    /// The line itself, as both frontends print it.
    pub text: String,
    /// The attempt this line belongs to, when it is one of an attempt's own step lines — the
    /// detail screen's own way of knowing which attempt's output `l` should open on this line;
    /// `show` prints every line's `text` and ignores this.
    pub attempt: Option<u32>,
}

fn line(text: impl Into<String>) -> DetailLine {
    DetailLine {
        text: text.into(),
        attempt: None,
    }
}

/// Every fact `detail` carries, as the detail screen and `show` print it: the task's own
/// fields first, then every attempt, newest first, with every step in full.
#[must_use]
pub fn detail_lines(detail: &TaskDetail) -> Vec<DetailLine> {
    let mut lines = task_lines(detail);
    lines.extend(attempts_lines(detail));
    lines
}

/// Source: `provider`/`model` own-or-inherited.
fn source(is_own: bool) -> &'static str {
    if is_own { "own" } else { "inherited" }
}

/// The task's own fields: id, status, title, kind, provider, model, links, body and criteria.
fn task_lines(detail: &TaskDetail) -> Vec<DetailLine> {
    let task = &detail.task;
    let outcome = detail.status.as_ref().map(|entry| entry.attempt.outcome);
    let mut lines = vec![
        line(format!(
            "#{} {}",
            task.id,
            task_status(task.status, outcome)
        )),
        line(format!("Title: {}", task.title)),
    ];
    if let Some(mark) = detail.done_by_user.as_ref() {
        lines.push(line(done_mark_text(mark)));
    }
    lines.push(line(format!("Kind: {}", task.kind)));
    lines.extend(provider_model_lines(detail));
    lines.extend(listed("Links:", &task.links, |link| format!("  {link}")));
    lines.extend(listed(
        "Body:",
        &task.body.lines().collect::<Vec<_>>(),
        |text| format!("  {text}"),
    ));
    lines.push(line("Criteria:"));
    lines.extend(
        task.criteria
            .iter()
            .map(|criterion| line(format!("  - {criterion}"))),
    );
    lines
}

/// `detail`'s provider and model, each said own or inherited.
fn provider_model_lines(detail: &TaskDetail) -> Vec<DetailLine> {
    let model = if detail.model.is_empty() {
        "none"
    } else {
        &detail.model
    };
    vec![
        line(format!(
            "Provider: {} ({})",
            detail.provider,
            source(detail.provider_is_own)
        )),
        line(format!("Model: {model} ({})", source(detail.model_is_own))),
    ]
}

/// `heading`, then one indented line per item of `items` through `shown`, or `  none` when
/// there are none.
fn listed<T>(heading: &str, items: &[T], shown: impl Fn(&T) -> String) -> Vec<DetailLine> {
    let mut lines = vec![line(heading)];
    if items.is_empty() {
        lines.push(line("  none"));
    } else {
        lines.extend(items.iter().map(|item| line(shown(item))));
    }
    lines
}

/// Every attempt's own lines, newest first: the current one, then its history, latest to
/// earliest.
fn attempts_lines(detail: &TaskDetail) -> Vec<DetailLine> {
    let Some(entry) = &detail.status else {
        return Vec::new();
    };
    let mut lines = vec![line("")];
    lines.extend(attempt_lines(&entry.attempt, true));
    for earlier in entry.history.iter().rev() {
        lines.push(line(""));
        lines.extend(attempt_lines(earlier, false));
    }
    lines
}

/// One attempt's own heading and every step it ran, in full.
fn attempt_lines(attempt: &AttemptLine, latest: bool) -> Vec<DetailLine> {
    let heading = if latest {
        format!("Attempt {} (latest)", attempt.number)
    } else {
        format!("Attempt {}", attempt.number)
    };
    let mut lines = vec![DetailLine {
        text: heading,
        attempt: Some(attempt.number),
    }];
    lines.extend(attempt.steps.iter().map(|step| DetailLine {
        text: format!("  {}", step_detail_text(step)),
        attempt: Some(attempt.number),
    }));
    lines
}

/// One step, every fact it carries in one line: provider, model, time, outcome, the routed
/// verdict, session, limit facts, tokens and cost, and the whole reason — nothing elided.
fn step_detail_text(step: &StepLine) -> String {
    let mut parts = vec![
        step.step.clone(),
        format!("provider: {}", step.provider.as_deref().unwrap_or("-")),
    ];
    if let Some(model) = &step.model {
        parts.push(format!("model: {model}"));
    }
    parts.push(format!("time: {}s", step.time_spent.as_secs()));
    parts.push(format!("outcome: {}", outcome(step.outcome)));
    if let Some(routed) = step.routed {
        parts.push(routed_text(routed));
    }
    let session = session_suffix(step.session.as_deref());
    if !session.is_empty() {
        parts.push(session);
    }
    if let Some(wait) = &step.limit_wait {
        parts.push(limit_wait_text(wait));
    }
    if let Some(warning) = &step.limit_warning {
        parts.push(limit_warning_text(warning));
    }
    if let Some(usage) = step_usage_text(step) {
        parts.push(usage);
    }
    if let Some(more_time) = step.more_time {
        parts.push(more_time_text(more_time));
    }
    if let Some(reason) = reason(step) {
        parts.push(format!("reason: {reason}"));
    }
    parts.join("  ·  ")
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use ktask_core::{
        AttemptOutcome, DoneMark, Outcome, StatusEntry, Task, TaskId, TaskKind, TaskStatus, Usage,
    };

    use super::*;

    fn task() -> Task {
        Task {
            id: TaskId(7),
            position: 1,
            title: "Fix the frobnicator".to_owned(),
            body: String::new(),
            criteria: vec!["it compiles".to_owned()],
            kind: TaskKind::Agent,
            links: vec![],
            provider: None,
            model: None,
            status: TaskStatus::Pending,
            created_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn step(name: &str, outcome: AttemptOutcome, reason: Option<&str>) -> StepLine {
        StepLine {
            step: name.to_owned(),
            provider: Some("echo".to_owned()),
            model: None,
            session: None,
            time_spent: Duration::from_secs(3),
            outcome,
            reason: reason.map(str::to_owned),
            waiting: None,
            limit_wait: None,
            limit_warning: None,
            usage: Usage::default(),
            routed: None,
            more_time: None,
        }
    }

    fn attempt(number: u32, steps: Vec<StepLine>) -> AttemptLine {
        let last = steps.last().cloned().unwrap();
        AttemptLine {
            number,
            step: last.step,
            provider: last.provider,
            model: last.model,
            session: last.session,
            time_spent: last.time_spent,
            outcome: last.outcome,
            reason: last.reason,
            waiting: None,
            limit_wait: None,
            limit_warning: None,
            output_activity: None,
            steps,
            usage: Usage::default(),
            routed: None,
            more_time: None,
        }
    }

    fn texts(lines: &[DetailLine]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }

    #[test]
    fn a_never_attempted_task_shows_its_own_fields_and_no_attempts() {
        let detail = TaskDetail {
            task: task(),
            provider: "echo".to_owned(),
            provider_is_own: false,
            model: String::new(),
            model_is_own: false,
            status: None,
            done_by_user: None,
        };
        let lines = detail_lines(&detail);
        assert_eq!(texts(&lines)[0], "#7 pending");
        assert_eq!(texts(&lines)[1], "Title: Fix the frobnicator");
        assert!(texts(&lines).contains(&"Provider: echo (inherited)"));
        assert!(texts(&lines).contains(&"Model: none (inherited)"));
        assert!(texts(&lines).contains(&"Links:"));
        assert!(texts(&lines).contains(&"  none"));
        assert!(!texts(&lines).iter().any(|text| text.starts_with("Attempt")));
    }

    #[test]
    fn a_tasks_own_provider_and_model_are_said_so() {
        let detail = TaskDetail {
            task: task(),
            provider: "codex".to_owned(),
            provider_is_own: true,
            model: "gpt-5".to_owned(),
            model_is_own: true,
            status: None,
            done_by_user: None,
        };
        let lines = detail_lines(&detail);
        let texts = texts(&lines);
        assert!(texts.contains(&"Provider: codex (own)"));
        assert!(texts.contains(&"Model: gpt-5 (own)"));
    }

    #[test]
    fn a_long_reason_is_carried_whole_with_nothing_cut() {
        let long = "x".repeat(200);
        let steps = vec![step("implementation", AttemptOutcome::Failed, Some(&long))];
        let detail = TaskDetail {
            task: task(),
            provider: "echo".to_owned(),
            provider_is_own: false,
            model: String::new(),
            model_is_own: false,
            status: Some(StatusEntry {
                task: TaskId(7),
                title: "Fix the frobnicator".to_owned(),
                status: TaskStatus::Failed,
                attempt: attempt(1, steps),
                history: vec![],
                done_by_user: None,
            }),
            done_by_user: None,
        };
        let lines = detail_lines(&detail);
        let step_line = lines
            .iter()
            .find(|line| line.text.contains("implementation"))
            .unwrap();
        assert!(step_line.text.contains(&long));
        assert_eq!(step_line.attempt, Some(1));
    }

    #[test]
    fn attempts_are_shown_newest_first_and_each_is_tagged_with_its_own_number() {
        let current = attempt(
            2,
            vec![step("implementation", AttemptOutcome::Passed, None)],
        );
        let history = vec![attempt(
            1,
            vec![step(
                "implementation",
                AttemptOutcome::Failed,
                Some("it broke"),
            )],
        )];
        let detail = TaskDetail {
            task: task(),
            provider: "echo".to_owned(),
            provider_is_own: false,
            model: String::new(),
            model_is_own: false,
            status: Some(StatusEntry {
                task: TaskId(7),
                title: "Fix the frobnicator".to_owned(),
                status: TaskStatus::Done,
                attempt: current,
                history,
                done_by_user: None,
            }),
            done_by_user: None,
        };
        let lines = detail_lines(&detail);
        let headings: Vec<_> = lines
            .iter()
            .filter(|line| line.text.starts_with("Attempt"))
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(headings, ["Attempt 2 (latest)", "Attempt 1"]);
    }

    #[test]
    fn a_done_mark_by_the_user_is_shown_with_its_reason_and_when() {
        let detail = TaskDetail {
            task: task(),
            provider: "echo".to_owned(),
            provider_is_own: false,
            model: String::new(),
            model_is_own: false,
            status: Some(StatusEntry {
                task: TaskId(7),
                title: "Fix the frobnicator".to_owned(),
                status: TaskStatus::Done,
                attempt: attempt(
                    1,
                    vec![step(
                        "implementation",
                        AttemptOutcome::Reported(Outcome::Done),
                        None,
                    )],
                ),
                history: vec![],
                done_by_user: None,
            }),
            done_by_user: Some(DoneMark {
                reason: "fixed by hand".to_owned(),
                at: SystemTime::UNIX_EPOCH,
            }),
        };
        let lines = detail_lines(&detail);
        let texts = texts(&lines);
        assert!(texts.iter().any(|text| text.contains("fixed by hand")));
    }
}
