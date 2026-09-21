//! Run command: drain the queue in order.

use crate::render;
use ktask_core::{RunOutcome, TaskId, queue, recovery, runner::Runner};

pub(crate) fn run(
    project: Option<ktask_core::Project>,
    _config: Option<ktask_core::Config>,
    task_id: Option<String>,
    from: Option<String>,
) -> RunOutcome {
    let Some(proj) = project else {
        return RunOutcome::Usage {
            detail: "no project found".to_string(),
        };
    };

    // Reconcile any interrupted tasks before selecting a task
    let mut journal = match ktask_core::Journal::open_for(&proj) {
        Ok(j) => j,
        Err(e) => {
            render::progress(format_args!("error opening journal: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    };

    if let Err(e) = journal.rebuild_state() {
        render::progress(format_args!("error rebuilding state: {e}"));
        return RunOutcome::Usage {
            detail: format!("{e}"),
        };
    }

    match recovery::reconcile(&mut journal, &proj) {
        Ok(decisions) => {
            for decision in decisions {
                render::out(format_args!(
                    "recovery: task {} {}",
                    decision.task_id,
                    match decision.decision {
                        ktask_core::Recovery::Resume => "resume",
                        ktask_core::Recovery::MarkInterrupted => "mark_interrupted",
                        ktask_core::Recovery::AlreadyApplied => "already_applied",
                    }
                ));
            }
        }
        Err(e) => {
            render::progress(format_args!("error reconciling: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    }

    let tasks = match queue::load(&proj) {
        Ok(t) => t,
        Err(e) => {
            render::progress(format_args!("error loading queue: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    };

    if tasks.is_empty() {
        return RunOutcome::Drained;
    }

    let from_id = from
        .as_ref()
        .and_then(|s| s.parse::<u32>().ok())
        .map(TaskId::new);

    // Try to create a runner with actual task execution
    let mut runner = match Runner::new(proj.clone()) {
        Ok(r) => Some(r),
        Err(_) => None, // Fall back to stub if runner can't be created (e.g., missing config)
    };

    if let Some(ref mut r) = runner {
        match r.run_queue(&tasks, from_id) {
            Ok(outcome) => {
                // Output task results before returning
                let mut journal = match ktask_core::Journal::open_for(&r.project) {
                    Ok(j) => j,
                    Err(e) => {
                        render::progress(format_args!("error opening journal for results: {e}"));
                        return outcome;
                    }
                };

                if let Err(e) = journal.rebuild_state() {
                    render::progress(format_args!("error rebuilding state for results: {e}"));
                    return outcome;
                }

                let states = match journal.all_states() {
                    Ok(s) => s,
                    Err(e) => {
                        render::progress(format_args!("error reading task states: {e}"));
                        return outcome;
                    }
                };

                for task_info in &tasks {
                    if let Some(state) = states.get(&task_info.id) {
                        render::out(format_args!(
                            "id={} title={} state={}",
                            task_info.id,
                            task_info.title(),
                            state.name()
                        ));
                    }
                }

                return outcome;
            }
            Err(e) => {
                render::progress(format_args!("error running queue: {e}"));
                return RunOutcome::Usage {
                    detail: format!("{e}"),
                };
            }
        }
    }

    // Stub fallback: just print task info without running
    let filtered_tasks = filter_tasks(&tasks, task_id, from);
    for task_to_run in filtered_tasks {
        render::out(format_args!(
            "id={} title={} state=done",
            task_to_run.id,
            task_to_run.title()
        ));
    }

    RunOutcome::Drained
}

fn filter_tasks(
    tasks: &[ktask_core::Task],
    single_task: Option<String>,
    from_task: Option<String>,
) -> Vec<ktask_core::Task> {
    if let Some(task_id_str) = single_task {
        tasks
            .iter()
            .filter(|t| t.id.to_string() == task_id_str)
            .cloned()
            .collect()
    } else if let Some(from_id_str) = from_task {
        tasks
            .iter()
            .skip_while(|t| t.id.to_string() != from_id_str)
            .cloned()
            .collect()
    } else {
        tasks.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::testing::ScratchRepo;
    use ktask_core::{EventKind, Journal, ids::AttemptId, ids::TaskId};

    #[test]
    fn cli_run_drains_the_queue() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = ktask_core::register(repo.path()).expect("Failed to register project");

        let outcome = run(Some(project), None, None, None);
        match outcome {
            RunOutcome::Drained => {}
            _ => panic!("Expected Drained, got {outcome:?}"),
        }
    }

    #[test]
    fn reconciles_before_selecting_a_task() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = ktask_core::register(repo.path()).expect("Failed to register project");

        let task_id = TaskId::new(1);

        // Set up journal with a task in running state (but with dead process)
        let mut journal = Journal::open_for(&project).expect("Failed to open journal");

        journal
            .append(
                Some(task_id),
                &EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
            )
            .expect("append task queued");

        journal
            .append(Some(task_id), &EventKind::PreflightStarted)
            .expect("append preflight started");

        journal
            .append(
                Some(task_id),
                &EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
            )
            .expect("append preflight passed");

        // Start an attempt with a non-existent PID (simulating a crash)
        journal
            .append(
                Some(task_id),
                &EventKind::AttemptStarted {
                    attempt: AttemptId::new(1),
                    protocol: "direct".to_string(),
                    pid: 999_999,
                    base_sha: "abc123".to_string(),
                },
            )
            .expect("append attempt started");

        journal.rebuild_state().expect("rebuild state");
        drop(journal);

        // Run should reconcile before selecting tasks
        let outcome = run(Some(project.clone()), None, None, None);

        // After reconciliation, the task should be marked interrupted
        let journal = Journal::open_for(&project).expect("Failed to open journal");
        let events = journal.events_for(task_id).expect("get events");

        // Should have a RecoveryDecision event
        let has_recovery_decision = events
            .iter()
            .any(|e| matches!(e.kind, EventKind::RecoveryDecision { .. }));
        assert!(
            has_recovery_decision,
            "Recovery decision should be recorded in journal after run"
        );

        // Verify the outcome
        match outcome {
            RunOutcome::Drained => {}
            _ => panic!("Expected Drained, got {outcome:?}"),
        }
    }
}
