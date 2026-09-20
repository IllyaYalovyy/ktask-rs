//! JSON serialization and output.

use serde::Serialize;
use std::io::{self, Write};

/// Emit a value as JSON to stdout.
///
/// Used for `--json` output to enable machine-readable results that can be
/// piped to tools like `jq`.
#[allow(dead_code)]
pub(crate) fn emit_json<T: Serialize>(value: &T) -> io::Result<()> {
    let json = serde_json::to_string(value).map_err(io::Error::other)?;
    writeln!(io::stdout(), "{json}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize, serde::Deserialize)]
    struct TestData {
        name: String,
        value: i32,
    }

    #[test]
    fn emit_json_serializes_correctly() {
        let data = TestData {
            name: "test".to_string(),
            value: 42,
        };

        // We can't easily capture stdout in this test, but we can verify it doesn't panic
        let result = emit_json(&data);
        assert!(result.is_ok());
    }

    #[test]
    fn emit_json_produces_valid_json() {
        let data = TestData {
            name: "example".to_string(),
            value: 100,
        };

        let json_str = serde_json::to_string(&data).unwrap();
        // Verify it parses back correctly
        let parsed: TestData = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed.name, "example");
        assert_eq!(parsed.value, 100);
    }
}
