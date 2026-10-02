//! Keeping a provider session's transcript as a plain file, appended to each time an
//! invocation under that session runs.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use ktask_core::{SessionLog, SessionLogError};

/// A session log backed by plain files: one per session, appended to, never truncated.
#[derive(Debug, Default)]
pub struct FileSessionLog;

impl SessionLog for FileSessionLog {
    fn append(&self, path: &Path, content: &[u8]) -> Result<(), SessionLogError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                SessionLogError::new(format!(
                    "cannot create the directory {} for a session's transcript: {e}",
                    parent.display()
                ))
            })?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| {
                SessionLogError::new(format!(
                    "cannot open the session transcript {}: {e}",
                    path.display()
                ))
            })?;
        file.write_all(content).map_err(|e| {
            SessionLogError::new(format!(
                "cannot write the session transcript {}: {e}",
                path.display()
            ))
        })
    }

    fn write_prompt(&self, path: &Path, prompt: &str) -> Result<(), SessionLogError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                SessionLogError::new(format!(
                    "cannot create the directory {} for a step's prompt scratch file: {e}",
                    parent.display()
                ))
            })?;
        }
        std::fs::write(path, prompt).map_err(|e| {
            SessionLogError::new(format!(
                "cannot write the prompt scratch file {}: {e}",
                path.display()
            ))
        })
    }

    fn remove_prompt(&self, path: &Path) -> Result<(), SessionLogError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SessionLogError::new(format!(
                "cannot remove the prompt scratch file {}: {e}",
                path.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn appending_to_a_session_with_no_directory_yet_creates_it_and_the_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sessions").join("the-session.log");
        FileSessionLog.append(&path, b"first\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");
    }

    #[test]
    fn a_second_append_adds_to_the_first_rather_than_replacing_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("the-session.log");
        FileSessionLog.append(&path, b"first\n").unwrap();
        FileSessionLog.append(&path, b"second\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn writing_a_prompt_with_no_directory_yet_creates_it_and_the_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("prompts").join("1-1-implementation.prompt");
        FileSessionLog
            .write_prompt(&path, "the whole prompt")
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "the whole prompt");
    }

    #[test]
    fn a_second_write_replaces_the_first_rather_than_appending_to_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("1-1-implementation.prompt");
        FileSessionLog.write_prompt(&path, "first").unwrap();
        FileSessionLog.write_prompt(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn removing_a_prompt_scratch_file_deletes_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("1-1-implementation.prompt");
        FileSessionLog.write_prompt(&path, "first").unwrap();
        FileSessionLog.remove_prompt(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn removing_a_prompt_scratch_file_that_is_already_gone_succeeds() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("never-written.prompt");
        FileSessionLog.remove_prompt(&path).unwrap();
    }
}
