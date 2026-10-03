//! Reading the timestamp of an attempt's append-only provider-output file.

use std::path::PathBuf;
use std::time::SystemTime;

use ktask_core::{AttemptOutput, TaskId};

/// Filesystem-backed output activity for one project's attempts.
#[derive(Debug, Clone)]
pub struct FileAttemptOutput {
    outputs_dir: PathBuf,
}

impl FileAttemptOutput {
    /// Output activity read from files under `outputs_dir`.
    #[must_use]
    pub fn new(outputs_dir: PathBuf) -> Self {
        Self { outputs_dir }
    }
}

impl AttemptOutput for FileAttemptOutput {
    fn last_output_at(&self, task: TaskId, attempt: u32) -> Option<SystemTime> {
        let path = self.outputs_dir.join(format!("{task}-{attempt}.log"));
        let metadata = std::fs::metadata(path).ok()?;
        (metadata.len() > 0)
            .then(|| metadata.modified().ok())
            .flatten()
    }
}
