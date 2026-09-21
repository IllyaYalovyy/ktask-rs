//! Control signals: pause, interrupt, cancel from external terminals.

use crate::{Project, Result};
use std::path::PathBuf;

/// Check if a pause signal has been written to the project state directory.
///
/// Returns true if the pause signal file exists.
///
/// # Errors
///
/// This function cannot currently fail and always returns Ok.
pub fn check_pause(project: &Project) -> Result<bool> {
    let signal_path = pause_signal_path(project);
    Ok(signal_path.exists())
}

/// Check if an interrupt signal has been written to the project state directory.
///
/// Returns true if the interrupt signal file exists.
///
/// # Errors
///
/// This function cannot currently fail and always returns Ok.
pub fn check_interrupt(project: &Project) -> Result<bool> {
    let signal_path = interrupt_signal_path(project);
    Ok(signal_path.exists())
}

/// Write a pause signal to the project state directory.
///
/// The pause signal tells the runner to pause after the current task
/// reaches a safe boundary (between phases).
///
/// # Errors
///
/// Returns an error if the signal file cannot be written to the state directory.
pub fn write_pause(project: &Project) -> Result<()> {
    let signal_path = pause_signal_path(project);
    std::fs::write(&signal_path, b"pause")?;
    Ok(())
}

/// Write an interrupt signal to the project state directory.
///
/// The interrupt signal tells the runner to terminate the current attempt
/// immediately, leaving durable resumable state.
///
/// # Errors
///
/// Returns an error if the signal file cannot be written to the state directory.
pub fn write_interrupt(project: &Project) -> Result<()> {
    let signal_path = interrupt_signal_path(project);
    std::fs::write(&signal_path, b"interrupt")?;
    Ok(())
}

/// Clear the pause signal file.
///
/// # Errors
///
/// Returns an error if the signal file cannot be removed from the state directory.
pub fn clear_pause(project: &Project) -> Result<()> {
    let signal_path = pause_signal_path(project);
    if signal_path.exists() {
        std::fs::remove_file(signal_path)?;
    }
    Ok(())
}

/// Clear the interrupt signal file.
///
/// # Errors
///
/// Returns an error if the signal file cannot be removed from the state directory.
pub fn clear_interrupt(project: &Project) -> Result<()> {
    let signal_path = interrupt_signal_path(project);
    if signal_path.exists() {
        std::fs::remove_file(signal_path)?;
    }
    Ok(())
}

/// Get the path to the pause signal file.
fn pause_signal_path(project: &Project) -> PathBuf {
    project.state_dir.join("pause.signal")
}

/// Get the path to the interrupt signal file.
fn interrupt_signal_path(project: &Project) -> PathBuf {
    project.state_dir.join("interrupt.signal")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ScratchRepo;

    #[test]
    fn check_pause_returns_false_when_no_signal() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = crate::register(repo.path()).expect("Failed to register project");

        let result = check_pause(&project).expect("check pause");
        assert!(!result);
    }

    #[test]
    fn write_pause_creates_signal_file() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = crate::register(repo.path()).expect("Failed to register project");

        write_pause(&project).expect("write pause");
        let result = check_pause(&project).expect("check pause");
        assert!(result);
    }

    #[test]
    fn clear_pause_removes_signal_file() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = crate::register(repo.path()).expect("Failed to register project");

        write_pause(&project).expect("write pause");
        clear_pause(&project).expect("clear pause");
        let result = check_pause(&project).expect("check pause");
        assert!(!result);
    }

    #[test]
    fn check_interrupt_returns_false_when_no_signal() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = crate::register(repo.path()).expect("Failed to register project");

        let result = check_interrupt(&project).expect("check interrupt");
        assert!(!result);
    }

    #[test]
    fn write_interrupt_creates_signal_file() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = crate::register(repo.path()).expect("Failed to register project");

        write_interrupt(&project).expect("write interrupt");
        let result = check_interrupt(&project).expect("check interrupt");
        assert!(result);
    }

    #[test]
    fn clear_interrupt_removes_signal_file() {
        let repo = ScratchRepo::new().expect("Failed to create test repo");
        let project = crate::register(repo.path()).expect("Failed to register project");

        write_interrupt(&project).expect("write interrupt");
        clear_interrupt(&project).expect("clear interrupt");
        let result = check_interrupt(&project).expect("check interrupt");
        assert!(!result);
    }
}
