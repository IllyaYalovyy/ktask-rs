//! The terminal shell, exercised as a real process.
//!
//! A panic and a non-terminal stdout are both facts about a whole process, so
//! each is checked by re-running this test binary as a child with a marker in
//! the environment and reading what it wrote. The child bodies are ordinary
//! tests that do nothing unless that marker is set.

use ktask_tui::App;
use ktask_tui::terminal::{install_panic_hook, is_not_a_terminal, run};
use std::fs::File;
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;

const MARKER: &str = "KTASK_TUI_TERMINAL_CHILD";

/// Sequences a terminal is restored with: leave the alternate screen, then
/// show the cursor.
const RESTORE: &str = "\x1b[?1049l\x1b[?25h";

fn child_mode(mode: &str) -> bool {
    std::env::var(MARKER).is_ok_and(|m| m == mode)
}

fn rerun(test: &str, mode: &str) -> std::io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(MARKER, mode);
    Ok(command)
}

#[test]
fn terminal_child_panics_after_installing_the_hook() {
    if !child_mode("panic") {
        return;
    }
    install_panic_hook();
    panic!("boom from the terminal child");
}

#[test]
fn terminal_panic_restores_the_screen_before_the_message_is_printed() {
    let path = std::env::temp_dir().join(format!("ktask-tui-terminal-{}", std::process::id()));
    let file = File::create(&path).expect("create");
    // One file behind both stdout and stderr keeps their relative order.
    let mut child = rerun("terminal_child_panics_after_installing_the_hook", "panic")
        .expect("test binary path");
    let status = child
        .stdout(file.try_clone().expect("dup"))
        .stderr(file)
        .status()
        .expect("spawn child");
    assert!(!status.success(), "the child was meant to panic");

    let written = String::from_utf8_lossy(&std::fs::read(&path).expect("read")).into_owned();
    let _ = std::fs::remove_file(&path);
    let restored = written
        .find(RESTORE)
        .unwrap_or_else(|| panic!("no restore sequence in {written:?}"));
    let message = written
        .find("boom from the terminal child")
        .unwrap_or_else(|| panic!("panic message was lost: {written:?}"));
    assert!(
        restored < message,
        "the message was printed on the alternate screen: {written:?}"
    );
}

#[test]
fn terminal_child_runs_with_a_piped_stdout() {
    if !child_mode("piped") {
        return;
    }
    let (_tx, rx) = mpsc::channel();
    let outcome = run(App::new((80, 24)), rx);
    let mut err = std::io::stderr();
    match outcome {
        Err(e) if is_not_a_terminal(&e) => {
            writeln!(err, "REFUSED: {e}").expect("stderr");
        }
        other => writeln!(err, "UNEXPECTED: {other:?}").expect("stderr"),
    }
}

#[test]
fn terminal_run_refuses_a_piped_stdout_and_leaves_the_terminal_alone() {
    let Output { stdout, stderr, .. } = rerun("terminal_child_runs_with_a_piped_stdout", "piped")
        .expect("test binary path")
        .stdin(Stdio::null())
        .output()
        .expect("spawn child");
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(
        stderr.contains("REFUSED"),
        "stderr: {stderr}\nstdout: {stdout}"
    );
    assert!(stderr.contains("needs a terminal"), "{stderr}");
    assert!(
        !stdout.contains('\x1b'),
        "nothing may be drawn or restored when there is no terminal: {stdout:?}"
    );
}
