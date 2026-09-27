//! The person's text editor, run as a program.

use std::io::Write;
use std::process::Command;

use ktask_core::{Editor, EditorError};

/// An editor started from a command line, the way `git` starts `$EDITOR`.
#[derive(Debug, Clone)]
pub struct CommandEditor {
    command: String,
}

impl CommandEditor {
    /// An editor started by the shell command `command`, given the file to edit as its last
    /// argument. The command may carry arguments of its own, such as `emacs -nw`.
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
        }
    }
}

impl Editor for CommandEditor {
    fn edit(&self, text: &str) -> Result<String, EditorError> {
        let fail = |cause: String| EditorError::new(format!("editor `{}`: {cause}", self.command));
        let mut file = tempfile::Builder::new()
            .prefix("ktask-rs-task-")
            .suffix(".md")
            .tempfile()
            .map_err(|e| fail(format!("cannot create the file to edit: {e}")))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|e| fail(format!("cannot write {}: {e}", file.path().display())))?;
        let status = Command::new("sh")
            .arg("-c")
            .arg(format!("{} \"$@\"", self.command))
            .arg("sh")
            .arg(file.path())
            .status()
            .map_err(|e| fail(format!("cannot run it: {e}")))?;
        if !status.success() {
            return Err(fail(format!("it failed: {status}")));
        }
        let edited = std::fs::read(file.path())
            .map_err(|e| fail(format!("cannot read {}: {e}", file.path().display())))?;
        String::from_utf8(edited).map_err(|_| fail("it left text that is not UTF-8".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_is_given_the_text_in_a_file_and_what_it_leaves_is_returned() {
        let editor = CommandEditor::new("sed -i -e 's/a/b/g'");
        assert_eq!(editor.edit("banana\n"), Ok("bbnbnb\n".to_owned()));
    }

    #[test]
    fn an_editor_that_fails_is_an_error_naming_it_and_how_it_ended() {
        let error = CommandEditor::new("exit 3 #").edit("x").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("exit 3 #"), "{message}");
        assert!(message.contains("exit status: 3"), "{message}");
    }

    #[test]
    fn an_editor_that_does_not_exist_is_an_error() {
        let error = CommandEditor::new("no-such-editor-anywhere")
            .edit("x")
            .unwrap_err();
        assert!(error.to_string().contains("no-such-editor-anywhere"));
    }

    #[test]
    fn text_that_is_not_utf8_is_an_error() {
        let error = CommandEditor::new("printf '\\377' >")
            .edit("x")
            .unwrap_err();
        assert!(error.to_string().contains("UTF-8"), "{error}");
    }
}
