//! Add command: add a new task to the queue.

use crate::render;
use ktask_core::{Journal, Project, RunOutcome, TaskId, parse_plan};
use std::path::PathBuf;

pub(crate) fn run(project: Option<Project>, file: Option<PathBuf>) -> RunOutcome {
    let Some(proj) = project else {
        return RunOutcome::Drained;
    };

    let content = match read_task_content(file) {
        Ok(c) => c,
        Err(e) => {
            render::progress(format_args!("error: {e}"));
            return RunOutcome::Usage { detail: e };
        }
    };

    let mut tasks = match parse_plan(&content) {
        Ok(t) => t,
        Err(e) => {
            render::progress(format_args!("error: {e}"));
            return RunOutcome::Usage {
                detail: e.to_string(),
            };
        }
    };

    if tasks.is_empty() {
        render::progress(format_args!("error: no task found in content"));
        return RunOutcome::Usage {
            detail: "no task found".to_string(),
        };
    }

    for task in &tasks {
        if let Err(e) = task.validate() {
            render::progress(format_args!("error: {e}"));
            return RunOutcome::Usage {
                detail: e.to_string(),
            };
        }
    }

    let mut journal = match Journal::open_for(&proj) {
        Ok(j) => j,
        Err(e) => {
            render::progress(format_args!("error: {e}"));
            return RunOutcome::Usage {
                detail: e.to_string(),
            };
        }
    };

    let existing_tasks = match journal.tasks() {
        Ok(t) => t,
        Err(e) => {
            render::progress(format_args!("error: {e}"));
            return RunOutcome::Usage {
                detail: e.to_string(),
            };
        }
    };

    let next_id = if existing_tasks.is_empty() {
        1u32
    } else {
        existing_tasks.iter().map(|t| t.id.get()).max().unwrap_or(0) + 1
    };

    for (i, task) in tasks.iter_mut().enumerate() {
        task.id = TaskId::new(next_id + i as u32);
    }

    if let Err(e) = journal.put_tasks(&tasks) {
        render::progress(format_args!("error: {e}"));
        return RunOutcome::Usage {
            detail: e.to_string(),
        };
    }

    if let Some(first_task) = tasks.first() {
        render::out(format_args!("id={}", first_task.id));
    }
    RunOutcome::Drained
}

fn read_task_content(file: Option<PathBuf>) -> Result<String, String> {
    if let Some(path) = file {
        std::fs::read_to_string(&path).map_err(|e| format!("failed to read file: {e}"))
    } else {
        open_editor()
    }
}

fn open_editor() -> Result<String, String> {
    let editor = std::env::var("EDITOR").map_err(|_| "EDITOR not set".to_string())?;

    let tempfile =
        tempfile::NamedTempFile::new().map_err(|e| format!("failed to create temp file: {e}"))?;
    let temp_path = tempfile.path().to_path_buf();

    std::fs::write(&temp_path, create_template())
        .map_err(|e| format!("failed to write template: {e}"))?;

    let status = std::process::Command::new(&editor)
        .arg(&temp_path)
        .status()
        .map_err(|e| format!("failed to open editor: {e}"))?;

    if !status.success() {
        return Err("editor exited with error".to_string());
    }

    let content = std::fs::read_to_string(&temp_path)
        .map_err(|e| format!("failed to read edited file: {e}"))?;

    Ok(content)
}

fn create_template() -> &'static str {
    "## Task Title

**Outcome:** What should be achieved

**Done-when:** The criteria for completion

**Verify:** How to verify the task is complete

**Refs:** References and related resources
"
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::Task;
    use tempfile::TempDir;

    #[test]
    fn add_command_with_valid_task() {
        let temp = TempDir::new().unwrap();
        let state_dir = temp.path();

        let mut journal = Journal::open(&state_dir.join("journal.db")).expect("open journal");

        let task = Task {
            id: TaskId::new(1),
            status: ktask_core::TaskStatus::Pending,
            body: "## Test Task".to_string(),
            outcome: "Fix something".to_string(),
            done_when: "Tests pass".to_string(),
            verify: "cargo test".to_string(),
            refs: "Issue #123".to_string(),
        };

        let result = journal.put_tasks(&[task]);
        assert!(result.is_ok(), "Expected journal.put_tasks to succeed");
    }

    #[test]
    fn add_command_with_invalid_task() {
        let task = Task {
            id: TaskId::new(1),
            status: ktask_core::TaskStatus::Pending,
            body: "## Test Task".to_string(),
            outcome: String::new(),
            done_when: String::new(),
            verify: String::new(),
            refs: String::new(),
        };

        let result = task.validate();
        assert!(result.is_err(), "Expected validation to fail");

        if let Err(err) = result {
            let error_msg = format!("{err}");
            assert!(
                error_msg.contains("Outcome"),
                "Error should mention Outcome section"
            );
            assert!(
                error_msg.contains("Done-when"),
                "Error should mention Done-when section"
            );
            assert!(
                error_msg.contains("Verify"),
                "Error should mention Verify section"
            );
            assert!(
                error_msg.contains("Refs"),
                "Error should mention Refs section"
            );
        }
    }
}
