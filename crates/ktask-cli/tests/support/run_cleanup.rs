//! A test's guarantee that a `ktask-rs run` it caused to start — waited on directly, spawned
//! and left running, or started detached through the TUI's `r` (deliberately: quitting the
//! screen must not stop a run it started) — never outlives the test itself, whether it
//! passes, fails or panics.

use std::path::PathBuf;

use ktask_adapters::FileRunLock;
use ktask_core::RunLock;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

use super::support::Sandbox;

/// Where `project`'s run lock lives under `sandbox`'s state home — exactly where `ktask-rs
/// run` itself puts it, and its own process id along with it.
fn run_lock_file(sandbox: &Sandbox, project: &str) -> PathBuf {
    sandbox.state_dir().join(project).join("run.lock")
}

/// Kills outright whatever process presently holds `project`'s run lock. That is all a test
/// has to do: whatever that process itself started, tied to it or merely orphaned by it,
/// dies the moment it does — `ProcessCommands`'s own guard against everything a provider
/// starts takes it from there, however deep. Does nothing when nothing holds the lock.
pub(crate) fn kill_run_if_in_progress(sandbox: &Sandbox, project: &str) {
    let path = run_lock_file(sandbox, project);
    let Ok(true) = FileRunLock::new(path.clone()).in_progress() else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    if let Ok(pid) = content.trim().parse::<i32>() {
        let _ = signal::kill(Pid::from_raw(pid), Signal::SIGKILL);
    }
}
