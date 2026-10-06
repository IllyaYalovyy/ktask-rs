//! Reading an attempt's append-only provider-output files, one per agent step.

use std::path::PathBuf;
use std::time::SystemTime;

use ktask_core::{AttemptOutput, StepOutputStore, TaskId};

/// Filesystem-backed output for one project's attempts.
#[derive(Debug, Clone)]
pub struct FileAttemptOutput {
    outputs_dir: PathBuf,
}

impl FileAttemptOutput {
    /// Output read from files under `outputs_dir`.
    #[must_use]
    pub fn new(outputs_dir: PathBuf) -> Self {
        Self { outputs_dir }
    }
}

impl AttemptOutput for FileAttemptOutput {
    fn last_output_at(&self, task: TaskId, attempt: u32) -> Option<SystemTime> {
        let prefix = ktask_core::attempt_output_file_prefix(task, attempt);
        std::fs::read_dir(&self.outputs_dir)
            .ok()?
            .filter_map(Result::ok)
            .filter(|file| file.file_name().to_string_lossy().starts_with(&prefix))
            .filter_map(|file| file.metadata().ok())
            .filter(|metadata| metadata.len() > 0)
            .filter_map(|metadata| metadata.modified().ok())
            .max()
    }
}

impl StepOutputStore for FileAttemptOutput {
    fn read_step_output(&self, task: TaskId, attempt: u32, step: &str) -> Result<Vec<u8>, String> {
        let path = self
            .outputs_dir
            .join(ktask_core::step_output_file_name(task, attempt, step));
        match std::fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(format!(
                "cannot read attempt output {}: {error}",
                path.display()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_read_per_step_and_activity_is_the_latest_write_of_any_step_of_the_attempt() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(dir.path().join("4-2-implementation.log"), b"built").expect("write");
        std::fs::write(dir.path().join("4-2-review.log"), b"").expect("write");
        std::fs::write(dir.path().join("41-2-review.log"), b"other task").expect("write");
        let output = FileAttemptOutput::new(dir.path().to_path_buf());

        assert_eq!(
            output.read_step_output(TaskId(4), 2, "implementation"),
            Ok(b"built".to_vec())
        );
        assert_eq!(output.read_step_output(TaskId(4), 2, "testing"), Ok(vec![]));
        assert!(output.last_output_at(TaskId(4), 2).is_some());
        assert!(output.last_output_at(TaskId(4), 1).is_none());
        assert!(output.last_output_at(TaskId(5), 2).is_none());
    }
}
