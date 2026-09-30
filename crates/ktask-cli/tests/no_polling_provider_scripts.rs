//! B-22: no script a test hands a provider polls for a file to appear — one that has to wait
//! blocks on a fifo until the test writes to it — so a leaked provider never sits spinning,
//! and so a real failure to release it shows up as a hang, not as busywork nobody notices.

use std::path::Path;

/// No `.rs` file under `tests/` spells out the polling idiom this task replaced everywhere it
/// was found (`while [ ! -f ... ]; do sleep ...; done`), or the sleep interval that went with
/// it, spelled out on its own.
#[test]
fn no_provider_script_in_the_test_suite_polls_for_a_file() -> std::io::Result<()> {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut offenders = Vec::new();
    let mut stack = vec![tests_dir];
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
            // This file itself names the very idiom it forbids, to check for it.
            if path.file_name().and_then(std::ffi::OsStr::to_str)
                == Some("no_polling_provider_scripts.rs")
            {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            for needle in ["while [ ! -f", "sleep 0.02"] {
                if text.contains(needle) {
                    offenders.push(format!("{}: {needle:?}", path.display()));
                }
            }
        }
    }
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    Ok(())
}
