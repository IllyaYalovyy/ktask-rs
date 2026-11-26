//! An agent report as text, the same for `ktask agent show` and the TUI.
use crate::application::investigation::{Investigation, InvestigationState, ProgressKind, Report};

/// Agent and host text with every control character but newlines made visible.
fn clean(text: &str) -> String {
    text.trim()
        .chars()
        .flat_map(|c| match c {
            '\n' => vec!['\n'],
            '\t' => vec![' '],
            c if c.is_control() => c.escape_default().collect(),
            c => vec![c],
        })
        .collect()
}

fn one_line(text: &str) -> String {
    clean(text).replace('\n', " ")
}

pub fn usd(microusd: i64) -> String {
    format!("${:.2}", microusd as f64 / 1_000_000.0)
}

/// "Why did the check fail?", or what an unasked investigation explains.
pub fn question(investigation: &Investigation) -> String {
    investigation
        .question
        .as_deref()
        .map_or_else(|| "Explain the current state".to_owned(), one_line)
}

/// One list line: ID, state, when, task and question.
pub fn summary(investigation: &Investigation, now_ms: i64) -> String {
    format!(
        "#{} · {} · {}{} · {}",
        investigation.id,
        investigation.state.label(),
        crate::clock::format_local(investigation.started_at_ms, now_ms),
        investigation
            .task_id
            .map_or_else(String::new, |task| format!(" · task #{task}")),
        question(investigation)
    )
}

fn facts(investigation: &Investigation, now_ms: i64) -> String {
    let elapsed = investigation
        .finished_at_ms
        .unwrap_or(now_ms)
        .saturating_sub(investigation.started_at_ms)
        .unsigned_abs();
    let usage = &investigation.usage;
    let count = |value: Option<i64>| value.map_or_else(|| "?".into(), |n| n.to_string());
    let tokens = if usage.input_tokens.is_none() && usage.output_tokens.is_none() {
        "tokens not reported".to_owned()
    } else {
        format!(
            "tokens in {} · cache read {} · cache write {} · out {}",
            count(usage.input_tokens),
            count(usage.cache_read_tokens),
            count(usage.cache_write_tokens),
            count(usage.output_tokens)
        )
    };
    format!(
        "Question: {}\nTask: {}\nRequested by {}: {}\nModel: {}{}\nStarted {} · {} {}\nUsage: {tokens}\nCost: {} (limit {}) · time limit {}\n",
        question(investigation),
        investigation
            .task_id
            .map_or_else(|| "the current task".into(), |task| format!("#{task}")),
        investigation.requester_kind,
        one_line(&investigation.requester),
        one_line(&investigation.model.identifier()),
        investigation
            .model
            .effort
            .as_ref()
            .map_or_else(String::new, |effort| format!(" · {}", one_line(effort))),
        crate::clock::format_local(investigation.started_at_ms, now_ms),
        if investigation.finished_at_ms.is_some() {
            "took"
        } else {
            "running for"
        },
        super::duration(elapsed),
        usage
            .cost_microusd
            .map_or_else(|| "not reported".into(), usd),
        usd(investigation.cost_limit_microusd),
        super::duration(investigation.time_limit_ms.unsigned_abs()),
    )
}

/// What happened, the cause with what shows it, then the proposed fix.
fn report_text(report: &Report) -> String {
    let mut text = format!("WHAT HAPPENED\n{}\n\nCAUSE\n", clean(&report.what_happened));
    if let Some(defect) = &report.ktask_defect {
        text.push_str(&format!(
            "This is a defect in ktask itself: {}\n",
            clean(defect)
        ));
    }
    text.push_str(&format!("{}\n\nShown by:\n", clean(&report.cause)));
    for reference in &report.references {
        text.push_str(&format!(
            "- {} {}: {}{}\n",
            reference.kind,
            one_line(&reference.target),
            one_line(&reference.detail),
            if reference.verified == Some(false) {
                " [not found in ktask records]"
            } else {
                ""
            }
        ));
    }
    text.push_str("\nPROPOSED FIX (nothing was applied)\n");
    if report.ktask_defect.is_some() {
        text.push_str(
            "The fix belongs in ktask; a step marked workaround does not repair the defect.\n",
        );
    }
    if report.fix_steps.is_empty() {
        text.push_str("No step proposed.\n");
    }
    for (number, step) in report.fix_steps.iter().enumerate() {
        text.push_str(&format!(
            "{}. [{}]{} {}\n",
            number + 1,
            step.class.label(),
            if step.workaround {
                " (workaround, not a fix)"
            } else {
                ""
            },
            clean(&step.action)
        ));
    }
    text
}

fn progress_text(investigation: &Investigation) -> String {
    investigation
        .progress
        .iter()
        .map(|entry| {
            let mark = match entry.kind {
                ProgressKind::Status => "·",
                ProgressKind::Finding => "»",
                ProgressKind::Activity => " ",
            };
            format!("{mark} {}\n", clean(&entry.message))
        })
        .collect()
}

/// The whole record. Anything but a finished investigation says so before what it found,
/// so partial work is never read as a report.
pub fn document(investigation: &Investigation, now_ms: i64) -> String {
    let mut text = format!(
        "Agent report #{} · {}\n",
        investigation.id,
        investigation.state.label()
    );
    match (investigation.state, &investigation.report) {
        (InvestigationState::Finished, Some(report)) => {
            text.push_str(&format!(
                "\n{}\n{}\nHOW IT WAS FOUND\n{}",
                report_text(report),
                facts(investigation, now_ms),
                progress_text(investigation)
            ));
        }
        (InvestigationState::Running, _) => {
            if let Some(by) = &investigation.stop_requested_by {
                text.push_str(&format!("Stop requested by {}\n", one_line(by)));
            }
            text.push_str(&format!(
                "{}\nPROGRESS (no report yet)\n{}",
                facts(investigation, now_ms),
                progress_text(investigation)
            ));
        }
        _ => {
            text.push_str(&format!(
                "NOT A FINISHED REPORT: {}\n\n{}\nWHAT IT HAD FOUND\n{}",
                investigation
                    .reason
                    .as_deref()
                    .map_or_else(|| "it ended early".into(), clean),
                facts(investigation, now_ms),
                progress_text(investigation)
            ));
        }
    }
    text
}
