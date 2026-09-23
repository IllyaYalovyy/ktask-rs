//! `--json` emission: the machine-readable half of a state-reporting
//! command (`docs/CONTRACT.md` section 0 rule 3 — every command that
//! reports state accepts `--json`, and that output is a compatibility
//! surface). [`emit_json`] is the only way a command writes its `--json`
//! result: it always lands on stdout through [`crate::render::out`], one
//! compact line, and never shares a line — or a stream — with progress.

use crate::render::out;
use ktask_core::Result;
use serde::Serialize;

/// Serializes `value` to compact JSON and writes it to stdout as a single
/// line.
///
/// # Errors
///
/// Returns the serialization error and writes nothing: a command that gets
/// `Err` back from this has produced no partial output for a caller to
/// mistake for a complete result.
pub(crate) fn emit_json<T: Serialize>(value: &T) -> Result<()> {
    let text = serde_json::to_string(value)?;
    out(format_args!("{text}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::env;
    use std::process::Command;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Sample {
        id: u32,
        name: String,
    }

    /// A type whose `Serialize` impl always fails, standing in for any
    /// value `serde_json` cannot turn into JSON, so [`emit_json`]'s error
    /// path is exercised without depending on a specific type's quirks.
    struct AlwaysFailsToSerialize;

    impl Serialize for AlwaysFailsToSerialize {
        fn serialize<S>(&self, _serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("deliberately unserializable"))
        }
    }

    #[test]
    fn emit_json_fails_and_writes_nothing_for_an_unserializable_value() {
        let err = emit_json(&AlwaysFailsToSerialize).expect_err("must fail to serialize");
        assert!(err.to_string().contains("serde"), "unexpected error: {err}");
    }

    #[test]
    fn emit_json_writes_one_compact_line_to_stdout_and_nothing_to_stderr() {
        let exe = env::current_exe().expect("current test exe");
        let output = Command::new(exe)
            .args([
                "--exact",
                "--ignored",
                "--nocapture",
                "json::tests::emit_sample_json",
            ])
            .output()
            .expect("spawn child");
        let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
        let stderr = String::from_utf8(output.stderr).expect("stderr is utf8");

        // The child is libtest's own runner invoked for one `#[ignore]`d
        // test, so stdout also carries its "running 1 test" / "test ...
        // ok" narration; `emit_json`'s own line is the one starting with
        // `{`, and it must be the only such line.
        let json_lines: Vec<&str> = stdout
            .lines()
            .filter(|line| line.starts_with('{'))
            .collect();
        assert_eq!(
            json_lines.len(),
            1,
            "expected exactly one JSON line on stdout, got: {stdout:?}"
        );
        let json_line = *json_lines.first().expect("checked length above");
        let parsed: Sample = serde_json::from_str(json_line)
            .unwrap_or_else(|e| panic!("not the expected JSON: {e}\nline: {json_line:?}"));
        assert_eq!(
            parsed,
            Sample {
                id: 7,
                name: "task".to_string()
            }
        );
        assert!(
            !stderr.contains('{'),
            "emit_json must never write to stderr, got: {stderr:?}"
        );
    }

    #[test]
    #[ignore = "invoked directly as a child process by \
                emit_json_writes_one_compact_line_to_stdout_and_nothing_to_stderr"]
    fn emit_sample_json() {
        emit_json(&Sample {
            id: 7,
            name: "task".to_string(),
        })
        .expect("serializable value must emit");
    }
}
