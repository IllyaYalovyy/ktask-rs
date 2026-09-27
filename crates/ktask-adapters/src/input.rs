//! Text read from a file or from standard input.

use std::io::Read;

/// The text of the file `source`, or of standard input when `source` is `-`.
///
/// # Errors
///
/// Fails, saying which source, when it cannot be read or is not UTF-8 text.
pub fn read_text(source: &str) -> Result<String, String> {
    let read = if source == "-" {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text).map(|_| text)
    } else {
        std::fs::read_to_string(source)
    };
    read.map_err(|e| {
        let from = if source == "-" {
            "standard input"
        } else {
            source
        };
        format!("cannot read {from}: {e}")
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn a_file_is_read_as_text() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tasks.json");
        std::fs::write(&path, "[\"é\"]").unwrap();
        assert_eq!(read_text(&path.to_string_lossy()), Ok("[\"é\"]".to_owned()));
    }

    #[test]
    fn a_file_that_is_missing_or_not_text_is_an_error_naming_it() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("missing.json").display().to_string();
        let error = read_text(&missing).unwrap_err();
        assert!(error.contains(&missing), "{error}");
        let binary = dir.path().join("binary.json");
        std::fs::write(&binary, [0xff, 0xfe]).unwrap();
        let error = read_text(&binary.to_string_lossy()).unwrap_err();
        assert!(error.contains("UTF-8"), "{error}");
    }
}
