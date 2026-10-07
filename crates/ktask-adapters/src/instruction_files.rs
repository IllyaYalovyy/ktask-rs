//! Reading the instruction files — `VISION.md` and a role's own — as plain files.

use std::path::Path;

use ktask_core::{InstructionFiles, InstructionFilesError};

/// Instruction files read from the filesystem.
#[derive(Debug, Default)]
pub struct FileInstructionFiles;

impl InstructionFiles for FileInstructionFiles {
    fn read(&self, path: &Path) -> Result<String, InstructionFilesError> {
        std::fs::read_to_string(path).map_err(|error| {
            let reason = match error.kind() {
                std::io::ErrorKind::NotFound => "no such file".to_owned(),
                std::io::ErrorKind::InvalidData => "not valid UTF-8 text".to_owned(),
                _ => error.to_string(),
            };
            InstructionFilesError::new(reason)
        })
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn a_file_is_read_exactly_as_it_is() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("VISION.md");
        std::fs::write(&path, "one\n\ntwo").unwrap();
        assert_eq!(FileInstructionFiles.read(&path).unwrap(), "one\n\ntwo");
    }

    #[test]
    fn a_missing_file_says_so() {
        let dir = TempDir::new().unwrap();
        let error = FileInstructionFiles
            .read(&dir.path().join("CODER.md"))
            .unwrap_err();
        assert_eq!(error.to_string(), "no such file");
    }

    #[test]
    fn a_directory_in_the_files_place_is_an_error() {
        let dir = TempDir::new().unwrap();
        assert!(FileInstructionFiles.read(dir.path()).is_err());
    }

    #[test]
    fn a_file_that_is_not_text_is_an_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("CODER.md");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(
            FileInstructionFiles.read(&path).unwrap_err().to_string(),
            "not valid UTF-8 text"
        );
    }
}
