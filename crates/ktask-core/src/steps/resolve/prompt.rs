//! Building the resolve step's own prompt: [`super::Resolve`]'s only job besides running it and
//! reading back what it decided.

use std::fmt::Write as _;
use std::path::Path;

use crate::DecideWhy;
use crate::route::Decision;
use crate::steps::append_findings;
use crate::steps::implementation::EarlierAttempt;
use crate::{AttemptToken, Finding, Task, TaskStatus};

/// Appends one `- attempt N: outcome — reason` line per entry of `attempts` to `prompt`, then
/// — a list, not a blob — one line per finding its review step gave it, when it gave any.
fn append_attempts(prompt: &mut String, attempts: &[EarlierAttempt]) {
    for attempt in attempts {
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
        append_findings(prompt, &attempt.findings);
    }
}

/// Appends `task`'s title, body and acceptance criteria to `prompt`, headed as a resolve
/// prompt.
fn append_header(prompt: &mut String, task: &Task) {
    let _ = writeln!(prompt, "# Resolve: {}", task.title);
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
}

/// Appends `diff` to `prompt`, fenced under its own heading.
fn append_diff(prompt: &mut String, diff: &str) {
    prompt.push_str("\n## What the task has changed so far\n\n```diff\n");
    prompt.push_str(diff);
    if !diff.is_empty() && !diff.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push_str("```\n");
}

/// Appends the exact `report` command, run through `binary_path`, for each possible decision
/// of attempt `token` — `retry` takes an optional `--model <name>`, to hand the task's next
/// attempt to a different model when this one keeps failing a cheaper one; `stop` ends the task
/// `failed`, for an attempt not worth retrying; `skip` ends it `skipped`, for a task that is no
/// longer the right thing to do at all; `supersede` ends it `superseded`, replacing it with the
/// smaller tasks of a JSON array in the same format `import` takes, for a task too large to
/// finish as written.
fn append_reporting(
    prompt: &mut String,
    token: &AttemptToken,
    binary_path: &Path,
    timed_out: bool,
) {
    let binary = binary_path.display();
    let more_time = if timed_out {
        " [--same-session] [--more-time <minutes>]"
    } else {
        ""
    };
    let _ = write!(
        prompt,
        "\n## Reporting\n\n\
         You may change files. When you are done, run exactly one of these, with the decision \
         that fits:\n\n\
         \x20\x20\x20\x20{binary} report --token {token} retry [--model <name>]{more_time}\n\
         \x20\x20\x20\x20{binary} report --token {token} stop --reason \"<why>\"\n\
         \x20\x20\x20\x20{binary} report --token {token} skip --reason \"<why>\"\n\
         \x20\x20\x20\x20{binary} report --token {token} supersede --tasks <file>\n"
    );
}

/// Appends what the router saw of the failure that came to the decider, when it kept any, with
/// the option only a timeout offers.
fn append_decision(prompt: &mut String, decision: Option<&Decision>) {
    let Some(decision) = decision else {
        return;
    };
    prompt.push_str("\n## Why this came to you\n\n");
    if let Some(detail) = &decision.detail {
        prompt.push_str(detail);
        prompt.push('\n');
    }
    if decision.why == DecideWhy::TimeLimit {
        prompt.push_str(
            "\nThe attempt may only need more time: `retry --same-session --more-time <minutes>` \
             resumes its session and raises that one next attempt's time limit by that many \
             minutes.\n",
        );
    }
}

/// The prompt for the resolve step of attempt `token` of `task`: its title, body and
/// acceptance criteria; every attempt so far, `earlier` then this one's own `current_status`,
/// `current_reason` and `current_findings`, each with its own outcome and why — a list, not a
/// blob, when a review ended it; `diff`, everything the task has changed since its first
/// attempt began; and the exact `report` command, run through `binary_path`, to run for each
/// possible decision.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_resolve_prompt(
    task: &Task,
    token: &AttemptToken,
    binary_path: &Path,
    earlier: &[EarlierAttempt],
    current_status: TaskStatus,
    current_reason: Option<&str>,
    current_findings: &[Finding],
    diff: &str,
    decision: Option<&Decision>,
) -> String {
    let mut prompt = String::new();
    append_header(&mut prompt, task);
    prompt.push_str("\n## Every attempt so far\n\n");
    append_attempts(&mut prompt, earlier);
    append_attempts(
        &mut prompt,
        &[EarlierAttempt {
            number: token.number,
            outcome: current_status.as_str().to_owned(),
            reason: current_reason.map(str::to_owned),
            findings: current_findings.to_vec(),
        }],
    );
    append_decision(&mut prompt, decision);
    append_diff(&mut prompt, diff);
    let timed_out = decision.is_some_and(|decision| decision.why == DecideWhy::TimeLimit);
    append_reporting(&mut prompt, token, binary_path, timed_out);
    prompt
}
