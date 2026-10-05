//! Turning an attempt's steps into the lines the queue screen draws, and windowing a long list
//! of them to fit — [`super`]'s own lowest-level work, pulled out of it so that file stays
//! within the workspace's file-length limit.

use crate::presentation;
use ktask_core::{OutputActivity, StepLine};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

use crate::widgets::elide;

/// One line per step of `steps`, in order — the same lines `status` prints for the same
/// attempt, from the same use case: step (named with `label` ahead of it, so every step line
/// says which attempt it belongs to), provider (`-` for a step the tool ran itself, which names
/// none), time spent, outcome, and the reason when there is one, cut to fit `width` with a
/// trailing `…` when it does not.
pub(super) fn step_lines_named(
    steps: &[StepLine],
    width: usize,
    label: &str,
    activity: Option<&OutputActivity>,
) -> Vec<Line<'static>> {
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            step_line(
                step,
                width,
                label,
                (index + 1 == steps.len()).then_some(activity).flatten(),
            )
        })
        .collect()
}

/// `step` as one line, given `width` and `label` — [`step_lines_named`]'s own per-step work.
fn step_line(
    step: &StepLine,
    width: usize,
    label: &str,
    activity: Option<&OutputActivity>,
) -> Line<'static> {
    let text = step_text(step, width, label);
    let text = match activity {
        Some(activity) => {
            let activity_text = presentation::activity(activity);
            format!(
                "{text} · {} {}",
                activity_text.indicator, activity_text.message
            )
        }
        None => text,
    };
    Line::styled(
        elide(&text, width),
        Style::new().add_modifier(Modifier::DIM),
    )
}

/// The non-live part of one queue step line, including its optional failure reason and limit
/// wait, before [`step_line`] adds provider-output activity.
fn step_text(step: &StepLine, width: usize, label: &str) -> String {
    let prefix = step_prefix(step, label);
    let text = presentation::reason(step).map_or_else(
        || prefix.clone(),
        |reason| {
            let budget = width.saturating_sub(prefix.chars().count() + 2);
            format!("{prefix}: {}", elide(&reason, budget))
        },
    );
    let text = match &step.limit_wait {
        Some(wait) => format!("{text} · {}", presentation::queue_limit_wait_text(wait)),
        None => text,
    };
    presentation::step_usage_text(step)
        .map(|usage| format!("{text} · {usage}"))
        .unwrap_or(text)
}

/// The fixed part of a queue step line, before an optional reason and end facts.
fn step_prefix(step: &StepLine, label: &str) -> String {
    let provider = step.provider.as_deref().unwrap_or("-");
    let seconds = step.time_spent.as_secs();
    let outcome = presentation::outcome(step.outcome);
    let shown_provider = step.model.as_deref().map_or_else(
        || provider.to_owned(),
        |model| format!("{provider} ({model})"),
    );
    let session = presentation::session_suffix(step.session.as_deref());
    let session = if session.is_empty() {
        String::new()
    } else {
        format!(" · {session}")
    };
    let limit_warning = step
        .limit_warning
        .as_ref()
        .map_or_else(String::new, |warning| {
            format!(" · {}", presentation::limit_warning_text(warning))
        });
    let prefix = format!(
        "      {label}{} · {shown_provider} · {seconds}s · {outcome}{limit_warning}{session}",
        step.step
    );
    prefix
}

/// `lines`, kept to at most `budget`: shown in full when they already fit; otherwise the
/// earliest are dropped in favour of one leading `…` line, so the tail — the most recently
/// finished steps, and the one still running — stays visible, and the cut is never silent.
pub(super) fn windowed(lines: Vec<Line<'static>>, budget: usize) -> Vec<Line<'static>> {
    if lines.len() <= budget {
        return lines;
    }
    let skip = lines.len() + 1 - budget;
    let mut shown = vec![Line::styled(
        "      …",
        Style::new().add_modifier(Modifier::DIM),
    )];
    shown.extend(lines.into_iter().skip(skip));
    shown
}
