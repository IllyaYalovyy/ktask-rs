//! The structural rules of docs/CODER.md's task on the CLI: one module per command,
//! `main.rs` holds only the parser and the dispatch, no function in `ktask-cli/src` is
//! longer than 40 lines, and a release build ships exactly one binary.

use std::path::Path;
use std::process::Command;

/// `main.rs` defines the parser (`Cli`, `Command` and its nested subcommand enums) and the
/// dispatch (`main`, `dispatch`) — nothing that reads a use case's arguments, calls it, or
/// renders its result. Those live one per command under `src/commands/`.
#[test]
fn main_rs_holds_only_the_parser_and_the_dispatch() -> std::io::Result<()> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let text = std::fs::read_to_string(&path)?;

    let top_level_fns: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("fn "))
        .collect();
    assert_eq!(
        top_level_fns,
        ["fn main() -> ExitCode {", "fn dispatch("],
        "main.rs should define only `main` and `dispatch`; every other function belongs in \
         its command's own module under src/commands/"
    );
    for forbidden in [
        "ktask_core::",
        "ktask_adapters::",
        "ktask_tui::",
        "render::",
    ] {
        assert!(
            !text.contains(forbidden),
            "main.rs names {forbidden}; wiring adapters and calling use cases belongs in a \
             command module, not the dispatcher"
        );
    }
    Ok(())
}

/// Counts the lines of every function in a `.rs` file, keyed by the line its signature
/// starts on. A naive brace count is enough here: this codebase's string literals balance
/// their `{`/`}` within one line (format placeholders), so per-line brace deltas stay
/// correct.
fn function_lengths(text: &str) -> Vec<(usize, usize)> {
    let mut lengths = Vec::new();
    let mut depth: usize = 0;
    let mut active: Option<(usize, usize)> = None;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if active.is_none()
            && (trimmed.starts_with("fn ")
                || trimmed.starts_with("pub fn ")
                || trimmed.starts_with("pub(crate) fn ")
                || trimmed.starts_with("async fn ")
                || trimmed.starts_with("pub async fn "))
        {
            active = Some((index + 1, depth));
        }
        depth += line.matches('{').count();
        depth -= line.matches('}').count();
        if let Some((start, entry_depth)) = active
            && depth <= entry_depth
        {
            lengths.push((start, index + 1 - start + 1));
            active = None;
        }
    }
    lengths
}

/// No function under `ktask-cli/src` — the parser, the dispatch, a command module, or the
/// shared error and render helpers — does so much that it cannot be reviewed as one piece.
#[test]
fn no_function_in_the_cli_source_is_longer_than_40_lines() -> std::io::Result<()> {
    let source_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut stack = vec![source_dir];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            for (start_line, length) in function_lengths(&text) {
                if length > 40 {
                    offenders.push(format!("{}:{start_line}: {length} lines", path.display()));
                }
            }
        }
    }
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    Ok(())
}

/// A release build of `ktask-cli` produces exactly one binary: `ktask-rs`. `panic_test` is
/// is a cargo example, which cargo builds for `cargo test` and never for `cargo build`, so it
/// never lands in a release build's output.
#[test]
fn a_release_build_produces_only_ktask_rs() -> std::io::Result<()> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "build",
            "--release",
            "--message-format=json",
            "-p",
            "ktask-cli",
        ])
        .current_dir(&workspace_root)
        .output()?;
    assert!(
        output.status.success(),
        "cargo build --release failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_no_warning(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut executables: Vec<String> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .filter_map(|message| message["executable"].as_str().map(str::to_owned))
        .filter_map(|path| {
            Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    executables.sort();
    executables.dedup();
    assert_eq!(executables, ["ktask-rs"]);
    Ok(())
}

/// Every cargo invocation prints no warning, including the manifest ones cargo prints before
/// it compiles anything, such as an ignored dependency.
#[test]
fn cargo_reads_the_manifests_without_a_warning() -> std::io::Result<()> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version=1", "--no-deps"])
        .current_dir(&workspace_root)
        .output()?;
    assert!(output.status.success());
    assert_no_warning(&output.stderr);
    Ok(())
}

fn assert_no_warning(stderr: &[u8]) {
    let stderr = String::from_utf8_lossy(stderr);
    assert!(!stderr.contains("warning"), "{stderr}");
}
