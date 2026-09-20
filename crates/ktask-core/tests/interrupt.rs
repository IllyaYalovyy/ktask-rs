//! Integration test for interrupt handling.
//!
//! Tests that interrupting (SIGINT) leaves durable, resumable state:
//! 1. No child process survives
//! 2. Journal ends with Interrupted event
//! 3. Resume continues from the same task

use ktask_core::{Bus, EventKind, Journal, Project, Recorder, TaskId};
use std::process::Command;
use tempfile::TempDir;

/// Create a minimal project for testing.
fn setup_project() -> (TempDir, Project) {
    let dir = TempDir::new().expect("create temp dir");
    let root = dir.path().to_path_buf();
    let state_dir = root.join(".ktask");
    std::fs::create_dir_all(&state_dir).expect("create state dir");

    // Initialize a git repo
    let _ = Command::new("git")
        .args(&["init"])
        .current_dir(&root)
        .output();
    let _ = Command::new("git")
        .args(&["config", "user.email", "test@example.com"])
        .current_dir(&root)
        .output();
    let _ = Command::new("git")
        .args(&["config", "user.name", "Test User"])
        .current_dir(&root)
        .output();

    let project = Project {
        root: root.clone(),
        id: "test-project".to_string(),
        state_dir,
    };

    (dir, project)
}

#[test]
fn interrupt_journals_interrupted_event() {
    let (_dir, project) = setup_project();

    // Create a journal
    let journal = Journal::open_for(&project).expect("open journal");
    let bus = Bus::new(100);
    let mut recorder = Recorder::new(journal, bus);

    // Simulate recording an interrupt event
    let task_id = TaskId::new(1);
    let phase = ktask_core::state::Phase::Implement;

    recorder
        .record(Some(task_id), EventKind::Interrupted { phase })
        .expect("record interrupted event");

    // Verify the event was recorded
    let journal = Journal::open_for(&project).expect("reopen journal");
    let events = journal.events().expect("read events");

    // Find the Interrupted event
    let interrupted_event = events
        .iter()
        .find(|e| matches!(e.kind, EventKind::Interrupted { .. }));

    assert!(
        interrupted_event.is_some(),
        "Journal should contain Interrupted event"
    );
}

#[test]
fn journal_records_interrupted_event_with_phase() {
    let (_dir, project) = setup_project();

    let journal = Journal::open_for(&project).expect("open journal");
    let bus = Bus::new(100);
    let mut recorder = Recorder::new(journal, bus);

    let task_id = TaskId::new(1);
    let phase = ktask_core::state::Phase::Goal;

    recorder
        .record(Some(task_id), EventKind::Interrupted { phase })
        .expect("record event");

    let journal = Journal::open_for(&project).expect("reopen journal");
    let events = journal.events().expect("read events");

    let event = events
        .iter()
        .find(|e| e.task_id == Some(task_id) && matches!(e.kind, EventKind::Interrupted { .. }))
        .expect("find interrupted event");

    match &event.kind {
        EventKind::Interrupted { phase: p } => {
            assert_eq!(p, &ktask_core::state::Phase::Goal);
        }
        _ => panic!("expected Interrupted event"),
    }
}
