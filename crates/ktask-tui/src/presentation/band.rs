//! The queue's run band, in the one line `status` and the queue screen share: what is running,
//! where it most recently stopped and why, with the one action that continues, or that it is
//! idle.

use std::time::Duration;

use jiff::Timestamp;
use ktask_core::{Routed, RunBand, RunningBand, StopKind, StoppedBand, TaskId, TaskStatus};

use super::{activity, routed_label, task_status, wait_text};

/// The one line a status band shows for `band`.
#[must_use]
pub fn run_band_text(band: &RunBand) -> String {
    match band {
        RunBand::Running(running) => running_text(running),
        RunBand::Stopped(stopped) => stopped_text(stopped),
        RunBand::Idle { pending } => idle_text(*pending),
    }
}

/// How long a running step has run so far, in minutes once it reaches one.
fn elapsed_text(time_spent: Duration) -> String {
    let secs = time_spent.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{} min", secs / 60)
    }
}

/// What a running attempt's own output or usage-limit wait says right now, when it says
/// anything.
fn live_activity_text(running: &RunningBand) -> Option<String> {
    match running.waiting {
        Some(wait) => Some(wait_text(wait)),
        None => running
            .output_activity
            .as_ref()
            .map(|value| activity(value).message),
    }
}

fn running_text(running: &RunningBand) -> String {
    let provider = running.provider.as_deref().unwrap_or("-");
    let model = running
        .model
        .as_deref()
        .map_or_else(String::new, |model| format!(" ({model})"));
    let elapsed = elapsed_text(running.time_spent);
    let activity =
        live_activity_text(running).map_or_else(String::new, |text| format!(" · {text}"));
    format!(
        "running: #{} {} · {provider}{model} · {elapsed}{activity}",
        running.task.0, running.step
    )
}

fn stopped_text(stopped: &StoppedBand) -> String {
    let at = stopped.at.map_or_else(String::new, |at| {
        Timestamp::try_from(at)
            .map(|at| format!(" {at}"))
            .unwrap_or_default()
    });
    let (summary, next) = cause_text(stopped.task, &stopped.kind);
    format!("run stopped{at}: {summary} · next: {next}")
}

/// The stop's own summary and the one action that continues past it, for every [`StopKind`]
/// but [`StopKind::Ended`], which [`ended_text`] carries the extra fields for.
fn cause_text(task: TaskId, kind: &StopKind) -> (String, String) {
    match kind {
        StopKind::Ended {
            status,
            reason,
            routed,
        } => ended_text(task, *status, reason.as_deref(), *routed),
        StopKind::EnvironmentFault { step, reason } => (
            format!("#{} {step} failed — {reason}", task.0),
            "fix the problem, then r to run".to_owned(),
        ),
        StopKind::HumanTask => (
            format!("#{} is a human task", task.0),
            format!("H to acknowledge #{}, then r to run", task.0),
        ),
        StopKind::Interrupted => (
            format!("#{} interrupted — the run was killed", task.0),
            format!("r to run, then t to retry #{}", task.0),
        ),
    }
}

/// [`cause_text`] for [`StopKind::Ended`]: the task's own ending status, the router's last
/// verdict when it gave one, and the reason, in that order — the same order `status` and the
/// queue screen already show them in.
fn ended_text(
    task: TaskId,
    status: TaskStatus,
    reason: Option<&str>,
    routed: Option<Routed>,
) -> (String, String) {
    let routed_suffix = routed.map_or_else(String::new, |routed| {
        format!(" (routed: {})", routed_label(routed))
    });
    let reason_suffix = reason.map_or_else(String::new, |reason| format!(" — {reason}"));
    let summary = format!(
        "#{} {}{routed_suffix}{reason_suffix}",
        task.0,
        task_status(status, None)
    );
    let next = if status == TaskStatus::Blocked {
        format!("answer the question, then A to answer #{}", task.0)
    } else {
        format!("fix the cause, then t to retry #{}", task.0)
    };
    (summary, next)
}

fn idle_text(pending: usize) -> String {
    if pending == 0 {
        "idle · nothing pending".to_owned()
    } else {
        format!("idle · {pending} pending · r to run")
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use ktask_core::{OutputActivity, Wait, WaitReason};

    use super::*;

    #[test]
    fn a_running_band_names_the_task_step_provider_model_and_elapsed_time() {
        let running = RunningBand {
            task: TaskId(3),
            step: "implementation".to_owned(),
            provider: Some("claude".to_owned()),
            model: Some("claude-sonnet-5".to_owned()),
            time_spent: Duration::from_mins(12),
            waiting: None,
            output_activity: Some(OutputActivity {
                last_output_at: None,
                silent_for: Duration::from_secs(20),
                active: false,
                may_be_stuck: false,
            }),
        };
        assert_eq!(
            run_band_text(&RunBand::Running(running)),
            "running: #3 implementation · claude (claude-sonnet-5) · 12 min · silent for 20 s"
        );
    }

    #[test]
    fn a_running_band_with_no_provider_or_model_still_reads_cleanly() {
        let running = RunningBand {
            task: TaskId(1),
            step: "implementation".to_owned(),
            provider: None,
            model: None,
            time_spent: Duration::from_secs(5),
            waiting: None,
            output_activity: None,
        };
        assert_eq!(
            run_band_text(&RunBand::Running(running)),
            "running: #1 implementation · - · 5s"
        );
    }

    #[test]
    fn a_running_band_waiting_on_a_usage_limit_shows_the_countdown_instead_of_activity() {
        let running = RunningBand {
            task: TaskId(1),
            step: "implementation".to_owned(),
            provider: Some("claude".to_owned()),
            model: None,
            time_spent: Duration::from_secs(5),
            waiting: Some(Wait {
                reason: WaitReason::UsageLimit,
                remaining: Duration::from_secs(70),
            }),
            output_activity: None,
        };
        assert_eq!(
            run_band_text(&RunBand::Running(running)),
            "running: #1 implementation · claude · 5s · the provider's usage limit was hit; resumes in 70s"
        );
    }

    #[test]
    fn a_stopped_band_for_a_failed_task_names_the_reason_the_time_and_the_retry_key() {
        let stopped = StoppedBand {
            task: TaskId(1),
            at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(60)),
            kind: StopKind::Ended {
                status: TaskStatus::Failed,
                reason: Some("it broke".to_owned()),
                routed: None,
            },
        };
        let text = run_band_text(&RunBand::Stopped(stopped));
        assert!(text.starts_with("run stopped 1970-01-01T00:01:00Z: #1 failed — it broke"));
        assert!(text.ends_with("next: fix the cause, then t to retry #1"));
    }

    #[test]
    fn a_stopped_band_for_a_blocked_task_names_the_answer_key() {
        let stopped = StoppedBand {
            task: TaskId(2),
            at: None,
            kind: StopKind::Ended {
                status: TaskStatus::Blocked,
                reason: Some("which path?".to_owned()),
                routed: None,
            },
        };
        assert_eq!(
            run_band_text(&RunBand::Stopped(stopped)),
            "run stopped: #2 blocked — which path? · next: answer the question, then A to answer #2"
        );
    }

    #[test]
    fn a_stopped_band_for_an_environment_fault_names_the_gate_and_its_reason() {
        let stopped = StoppedBand {
            task: TaskId(1),
            at: None,
            kind: StopKind::EnvironmentFault {
                step: "health check".to_owned(),
                reason: "exited with code 1".to_owned(),
            },
        };
        assert_eq!(
            run_band_text(&RunBand::Stopped(stopped)),
            "run stopped: #1 health check failed — exited with code 1 · next: fix the problem, then r to run"
        );
    }

    #[test]
    fn a_stopped_band_for_a_human_task_names_the_acknowledge_key() {
        let stopped = StoppedBand {
            task: TaskId(4),
            at: None,
            kind: StopKind::HumanTask,
        };
        assert_eq!(
            run_band_text(&RunBand::Stopped(stopped)),
            "run stopped: #4 is a human task · next: H to acknowledge #4, then r to run"
        );
    }

    #[test]
    fn a_stopped_band_for_an_interrupted_task_names_the_run_key() {
        let stopped = StoppedBand {
            task: TaskId(1),
            at: None,
            kind: StopKind::Interrupted,
        };
        assert_eq!(
            run_band_text(&RunBand::Stopped(stopped)),
            "run stopped: #1 interrupted — the run was killed · next: r to run, then t to retry #1"
        );
    }

    #[test]
    fn an_idle_band_names_how_many_are_pending_or_that_none_are() {
        assert_eq!(
            run_band_text(&RunBand::Idle { pending: 7 }),
            "idle · 7 pending · r to run"
        );
        assert_eq!(
            run_band_text(&RunBand::Idle { pending: 0 }),
            "idle · nothing pending"
        );
    }
}
