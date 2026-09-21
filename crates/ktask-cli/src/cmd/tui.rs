//! TUI command: launch the interactive interface.

use crate::render;
use ktask_core::{RunOutcome, recovery};

pub(crate) fn run(
    project: Option<ktask_core::Project>,
    _config: Option<ktask_core::Config>,
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

    RunOutcome::Drained
}
