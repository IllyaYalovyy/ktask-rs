//! Fakes of the instruction-files port.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

/// Instruction files held in memory, keyed by full path: a path that was never given reads as
/// missing.
#[derive(Debug, Default)]
pub(crate) struct FakeInstructionFiles {
    files: RefCell<std::collections::BTreeMap<PathBuf, String>>,
}

impl FakeInstructionFiles {
    /// Instruction files for every role, under `dir`: `VISION.md` holds `"vision"` and each
    /// role's file holds its own lower-case role name.
    pub(crate) fn complete_in(dir: &str) -> Self {
        let files = Self::default();
        for (name, text) in [
            ("VISION.md", "vision"),
            ("CODER.md", "coder"),
            ("REVIEWER.md", "reviewer"),
            ("TESTER.md", "tester"),
            ("RESOLVER.md", "resolver"),
        ] {
            files.put(&Path::new(dir).join(name), text);
        }
        files
    }

    /// Makes `path` read as `text`.
    pub(crate) fn put(&self, path: &Path, text: &str) {
        self.files
            .borrow_mut()
            .insert(path.to_owned(), text.to_owned());
    }

    /// Makes `path` read as missing again.
    pub(crate) fn remove(&self, path: &Path) {
        self.files.borrow_mut().remove(path);
    }
}

impl crate::InstructionFiles for FakeInstructionFiles {
    fn read(&self, path: &Path) -> Result<String, crate::InstructionFilesError> {
        self.files
            .borrow()
            .get(path)
            .cloned()
            .ok_or_else(|| crate::InstructionFilesError::new("no such file"))
    }
}

/// Instruction files that exist wherever they are looked for, each holding its own file name
/// and the word "text".
pub(crate) struct EveryInstructionFile;

impl crate::InstructionFiles for EveryInstructionFile {
    fn read(&self, path: &Path) -> Result<String, crate::InstructionFilesError> {
        let name = path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        Ok(format!("{name} text\n"))
    }
}
