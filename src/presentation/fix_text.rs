//! An agent fix as text, the same for `ktask agent show` and the TUI.
use super::report_text::{clean, facts, one_line, progress_text};
use crate::application::{
    fix::{ChangeKind, Fix, Probe, StepStatus},
    investigation::{Investigation, InvestigationState},
};

/// How a fix stands: only a verified one is done.
pub fn state(state: InvestigationState) -> &'static str {
    match state {
        InvestigationState::Finished => "Done",
        other => other.label(),
    }
}

/// What the fix was asked to do.
pub fn asked(investigation: &Investigation) -> String {
    investigation
        .question
        .as_deref()
        .map_or_else(|| "repair the project".to_owned(), one_line)
}

/// One list line: ID, state, when and what was asked.
pub fn summary(investigation: &Investigation, now_ms: i64) -> String {
    format!(
        "#{} · fix · {} · {} · {}",
        investigation.id,
        state(investigation.state),
        crate::clock::format_local(investigation.started_at_ms, now_ms),
        asked(investigation)
    )
}

fn result(passed: Option<bool>) -> &'static str {
    match passed {
        Some(true) => "passed",
        Some(false) => "failed",
        None => "not run",
    }
}

fn probe_line(probe: &Probe) -> String {
    format!(
        "- {} {}: before {} · after {}{}\n",
        probe.kind.label(),
        one_line(&probe.target),
        result(probe.passed_before),
        result(probe.passed_after),
        probe
            .detail
            .as_ref()
            .map_or_else(String::new, |detail| format!(" ({})", one_line(detail)))
    )
}

fn verification(investigation: &Investigation, fix: &Fix) -> String {
    let mut text = String::from("VERIFICATION\n");
    match &fix.verification {
        Some(verification) => text.push_str(&format!("{}\n", clean(verification))),
        None if investigation.state == InvestigationState::Running => {
            text.push_str("Not verified yet.\n");
        }
        None => text.push_str("Not verified.\n"),
    }
    for probe in &fix.probes {
        text.push_str(&probe_line(probe));
    }
    text
}

fn steps(fix: &Fix) -> String {
    let mut text = String::from("STEPS\n");
    if fix.steps.is_empty() {
        text.push_str("No step yet.\n");
    }
    for step in &fix.steps {
        let result = if step.result.is_empty() {
            String::new()
        } else {
            format!(" — {}", one_line(&step.result))
        };
        text.push_str(&format!(
            "{}. [{}] {}: {}{result}\n",
            step.sequence,
            step.status.label(),
            step.source,
            one_line(&step.action)
        ));
    }
    text
}

fn changes(fix: &Fix) -> String {
    let mut text = String::from("CHANGED (files, settings, where work went)\n");
    if fix.changes.is_empty() {
        text.push_str("Nothing recorded.\n");
    }
    for change in &fix.changes {
        text.push_str(&format!(
            "- {} {}: {}\n",
            change.kind.label(),
            one_line(&change.target),
            one_line(&change.detail)
        ));
    }
    text
}

/// What the owner still has to do or know.
fn remaining(investigation: &Investigation, fix: &Fix) -> String {
    let mut text = String::from("WHAT REMAINS\n");
    let mut empty = true;
    for step in fix
        .steps
        .iter()
        .filter(|step| matches!(step.status, StepStatus::Refused | StepStatus::LeftForOwner))
    {
        empty = false;
        let why = if step.result.is_empty() {
            String::new()
        } else {
            format!(" — {}", one_line(&step.result))
        };
        text.push_str(&format!(
            "- left for the owner: {}{why}\n",
            one_line(&step.action)
        ));
    }
    for probe in fix
        .probes
        .iter()
        .filter(|probe| probe.passed_after == Some(false))
    {
        empty = false;
        text.push_str(&format!(
            "- still failing: {} {}\n",
            probe.kind.label(),
            one_line(&probe.target)
        ));
    }
    let uncommitted: Vec<&str> = fix
        .changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Uncommitted)
        .map(|change| change.target.as_str())
        .collect();
    if !uncommitted.is_empty() {
        empty = false;
        text.push_str(&format!(
            "- this fix leaves uncommitted files: {}\n",
            one_line(&uncommitted.join(", "))
        ));
    }
    if empty {
        text.push_str(if investigation.state == InvestigationState::Running {
            "Not known yet.\n"
        } else {
            "Nothing.\n"
        });
    }
    text
}

/// The whole record. A fix that is not done says why before anything else.
pub fn document(investigation: &Investigation, fix: &Fix, now_ms: i64) -> String {
    let mut text = format!(
        "Agent fix #{} · {}\n",
        investigation.id,
        state(investigation.state)
    );
    if let Some(by) = &investigation.stop_requested_by
        && investigation.state == InvestigationState::Running
    {
        text.push_str(&format!("Stop requested by {}\n", one_line(by)));
    }
    if let Some(reason) = &investigation.reason {
        text.push_str(&format!("NOT DONE: {}\n", one_line(reason)));
    }
    if investigation.state != InvestigationState::Running {
        text.push_str(match (fix.continued, fix.resume) {
            (true, _) => "The queue was continued.\n",
            (false, true) => "The runner stays paused.\n",
            (false, false) => "The runner stays paused (ktask run continues).\n",
        });
    }
    if let Some(summary) = &fix.summary {
        text.push_str(&format!("\nAGENT SUMMARY\n{}\n", clean(summary)));
    }
    let progress = progress_text(investigation);
    let record = format!(
        "{}\n{}\n{}\n{}",
        verification(investigation, fix),
        steps(fix),
        changes(fix),
        remaining(investigation, fix)
    );
    let asked = format!(
        "{}: {}\nWhen verified: {}\n",
        fix.report_id.map_or("Instruction", |_| "From"),
        asked(investigation),
        if fix.resume {
            "continue the queue"
        } else {
            "leave the runner paused"
        }
    );
    let facts = facts(investigation, now_ms);
    // The question line of the shared facts is the fix's own request, shown above.
    let facts = facts
        .lines()
        .skip(2)
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    if investigation.state == InvestigationState::Running {
        // The newest progress is what a watching owner reads: it comes last.
        text.push_str(&format!("\n{asked}{facts}\n{record}\nPROGRESS\n{progress}"));
    } else {
        text.push_str(&format!(
            "\n{record}\n{asked}{facts}\nHOW IT WENT\n{progress}"
        ));
    }
    text
}
