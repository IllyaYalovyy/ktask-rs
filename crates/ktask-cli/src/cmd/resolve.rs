//! Resolve command: answer a `waiting_input` question.

use crate::render;
use ktask_core::{Journal, RunOutcome, TaskState, ids::TaskId, queue};
use std::io::Write;
use std::path::PathBuf;

pub(crate) fn run(
    project: Option<ktask_core::Project>,
    task: String,
    note: Option<String>,
) -> RunOutcome {
    let Some(proj) = project else {
        return RunOutcome::Usage {
            detail: "no project found".to_string(),
        };
    };

    let tasks = match queue::load(&proj) {
        Ok(t) => t,
        Err(e) => {
            render::progress(format_args!("error loading queue: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    };

    let mut journal = match Journal::open_for(&proj) {
        Ok(j) => j,
        Err(e) => {
            render::progress(format_args!("error opening journal: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    };

    let states = match journal.all_states() {
        Ok(s) => s,
        Err(e) => {
            render::progress(format_args!("error loading states: {e}"));
            return RunOutcome::Usage {
                detail: format!("{e}"),
            };
        }
    };

    let task_id = match task.parse::<u32>() {
        Ok(id) => TaskId::new(id),
        Err(_) => {
            return RunOutcome::Usage {
                detail: format!("invalid task id: {task}"),
            };
        }
    };

    let state = states.get(&task_id);

    match state {
        Some(TaskState::Paused {
            reason: ktask_core::state::PauseReason::Input,
            ..
        }) => {
            // Get the answer: either from --note or from $EDITOR
            let answer = match note {
                Some(n) => n,
                None => match get_input_from_editor() {
                    Ok(a) => a,
                    Err(e) => {
                        render::progress(format_args!("error opening editor: {e}"));
                        return RunOutcome::Usage {
                            detail: format!("{e}"),
                        };
                    }
                },
            };

            // Find next ADR number
            let adr_path = match find_next_adr_number(&proj) {
                Ok(p) => p,
                Err(e) => {
                    render::progress(format_args!("error finding ADR number: {e}"));
                    return RunOutcome::Usage {
                        detail: format!("{e}"),
                    };
                }
            };

            // Write the ADR
            if let Err(e) = write_adr(&adr_path, &answer) {
                render::progress(format_args!("error writing ADR: {e}"));
                return RunOutcome::Usage {
                    detail: format!("{e}"),
                };
            }

            // Journal the event
            let event = ktask_core::EventKind::DecisionResolved {
                adr_path: adr_path.clone(),
                answer: answer.clone(),
            };

            if let Err(e) = journal.append(Some(task_id), &event) {
                render::progress(format_args!("error journaling decision: {e}"));
                return RunOutcome::Usage {
                    detail: format!("{e}"),
                };
            }

            render::out(format_args!(
                "id={} title={} state=done",
                task_id,
                tasks
                    .iter()
                    .find(|t| t.id == task_id)
                    .map(|t| t.title())
                    .unwrap_or("Unknown")
            ));

            RunOutcome::Drained
        }
        _ => RunOutcome::Usage {
            detail: format!("task {task} is not waiting for input"),
        },
    }
}

fn get_input_from_editor() -> std::io::Result<String> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());

    let mut temp_file = tempfile::NamedTempFile::new()?;
    temp_file.write_all(b"# Enter your decision above\n")?;
    temp_file.flush()?;

    let output = std::process::Command::new(&editor)
        .arg(temp_file.path())
        .status()?;

    if !output.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("editor exited with code {:?}", output.code()),
        ));
    }

    let content = std::fs::read_to_string(temp_file.path())?;
    Ok(content
        .lines()
        .filter(|l| !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string())
}

fn find_next_adr_number(project: &ktask_core::Project) -> std::io::Result<PathBuf> {
    let adr_dir = project.root.join("docs/adr");

    // Find the highest numbered ADR
    let mut max_num = -1i32;

    if adr_dir.exists() {
        for entry in std::fs::read_dir(&adr_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|s| s.to_str()) == Some("md") {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if let Ok(num) = name.split('-').next().unwrap_or("0").parse::<i32>() {
                        max_num = max_num.max(num);
                    }
                }
            }
        }
    } else {
        std::fs::create_dir_all(&adr_dir)?;
    }

    let next_num = max_num + 1;
    Ok(adr_dir.join(format!("{:04}-decision.md", next_num)))
}

fn write_adr(path: &PathBuf, decision: &str) -> std::io::Result<()> {
    let now = time::OffsetDateTime::now_utc();
    let date = now
        .format(time::macros::format_description!("[year]-[month]-[day]"))
        .unwrap_or_else(|_| "2026-09-20".to_string());

    // Extract the ADR number from the filename (NNNN-decision.md -> NNNN)
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("0000");
    let adr_num = filename.split('-').next().unwrap_or("0000");

    let content = format!(
        "# {}. Decision\n\n\
         - **Status:** accepted\n\
         - **Date:** {}\n\n\
         ## Context\n\n\
         (context to be filled)\n\n\
         ## Decision\n\n\
         {}\n\n\
         ## Alternatives considered\n\n\
         (alternatives to be filled)\n\n\
         ## Consequences\n\n\
         (consequences to be filled)\n",
        adr_num, date, decision
    );

    std::fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::testing::ScratchRepo;

    #[test]
    fn cli_resolve_on_non_waiting_task_exits_2() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = ktask_core::register(repo.path()).expect("Failed to register project");

        let outcome = run(Some(project), "1".to_string(), Some("answer".to_string()));
        match outcome {
            RunOutcome::Usage { .. } => {}
            _ => panic!("Expected Usage (exit 2), got {outcome:?}"),
        }
    }
}
