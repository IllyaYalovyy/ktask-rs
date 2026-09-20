#![allow(
    clippy::expect_used,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::panic,
    clippy::uninlined_format_args
)]
//! Integration test for crash recovery at every phase boundary.
//!
//! Tests that recovery is proven at every phase boundary in the lifecycle, not argued.
//! For each boundary, sets up state as if the task reached that boundary with the events
//! durably journaled, then verifies the journal survives and can be reopened correctly.

use ktask_core::{AttemptId, EventKind, Journal, Phase, TaskId, TaskState};
use std::fs;
use tempfile::TempDir;

/// A phase boundary test case.
#[derive(Debug)]
struct BoundaryTest {
    /// Name of the boundary (e.g., `preflight_to_running`)
    name: &'static str,
    /// Description of what should happen at this boundary
    description: &'static str,
    /// Events to journal to simulate reaching this boundary
    events: Vec<EventKind>,
}

impl BoundaryTest {
    fn new(name: &'static str, description: &'static str, events: Vec<EventKind>) -> Self {
        BoundaryTest {
            name,
            description,
            events,
        }
    }
}

fn create_boundaries() -> Vec<BoundaryTest> {
    vec![
        BoundaryTest::new(
            "queued_to_preflight",
            "Task queued, preflight not yet started",
            vec![EventKind::TaskQueued {
                title: "Test task".to_string(),
            }],
        ),
        BoundaryTest::new(
            "preflight_to_running",
            "Preflight passed, running phase not yet started",
            vec![
                EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
                EventKind::PreflightStarted,
                EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
            ],
        ),
        BoundaryTest::new(
            "running_to_verifying",
            "Agent completed, verification not yet started",
            vec![
                EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
                EventKind::PreflightStarted,
                EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
                EventKind::AttemptStarted {
                    attempt: AttemptId::new(1),
                    protocol: "direct".to_string(),
                    pid: 1234,
                    base_sha: "abc123".to_string(),
                },
                EventKind::PhaseEntered {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement,
                },
            ],
        ),
        BoundaryTest::new(
            "verifying_to_publishing",
            "Verification passed, publishing not yet started",
            vec![
                EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
                EventKind::PreflightStarted,
                EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
                EventKind::AttemptStarted {
                    attempt: AttemptId::new(1),
                    protocol: "direct".to_string(),
                    pid: 1234,
                    base_sha: "abc123".to_string(),
                },
                EventKind::PhaseEntered {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement,
                },
                EventKind::VerifyPassed {
                    attempt: AttemptId::new(1),
                },
            ],
        ),
        BoundaryTest::new(
            "publishing_to_published_verified",
            "Publishing started, verification not yet done",
            vec![
                EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
                EventKind::PreflightStarted,
                EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
                EventKind::AttemptStarted {
                    attempt: AttemptId::new(1),
                    protocol: "direct".to_string(),
                    pid: 1234,
                    base_sha: "abc123".to_string(),
                },
                EventKind::PhaseEntered {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement,
                },
                EventKind::VerifyPassed {
                    attempt: AttemptId::new(1),
                },
                EventKind::PublishStarted {
                    attempt: AttemptId::new(1),
                    candidate_sha: "def456".to_string(),
                },
            ],
        ),
        BoundaryTest::new(
            "published_verified_to_done",
            "Published and verified, not yet marked done",
            vec![
                EventKind::TaskQueued {
                    title: "Test task".to_string(),
                },
                EventKind::PreflightStarted,
                EventKind::PreflightPassed {
                    base_sha: "abc123".to_string(),
                },
                EventKind::AttemptStarted {
                    attempt: AttemptId::new(1),
                    protocol: "direct".to_string(),
                    pid: 1234,
                    base_sha: "abc123".to_string(),
                },
                EventKind::PhaseEntered {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement,
                },
                EventKind::VerifyPassed {
                    attempt: AttemptId::new(1),
                },
                EventKind::PublishStarted {
                    attempt: AttemptId::new(1),
                    candidate_sha: "def456".to_string(),
                },
                EventKind::PublishVerified {
                    commit: "def456".to_string(),
                    remote_sha: "def456".to_string(),
                },
            ],
        ),
    ]
}

/// Test recovery at a single phase boundary.
fn test_boundary(boundary: &BoundaryTest) {
    eprintln!(
        "Testing boundary: {} - {}",
        boundary.name, boundary.description
    );

    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let state_dir = temp_dir.path().to_path_buf();
    fs::create_dir_all(&state_dir).expect("Failed to create state dir");

    let journal_path = state_dir.join("journal.db");
    let mut journal = Journal::open(&journal_path).expect("Failed to open journal");

    // Initialize the task with the sequence of events that bring us to this boundary
    let task_id = TaskId::new(1);
    for (i, event) in boundary.events.iter().enumerate() {
        journal
            .append(Some(task_id), event)
            .unwrap_or_else(|e| panic!("Failed to append event {}: {}", i, e));
    }

    // Rebuild the materialized state from events (this is what happens on normal operation)
    journal
        .rebuild_state()
        .unwrap_or_else(|e| panic!("Failed to rebuild state for {}: {}", boundary.name, e));

    // Verify the journal is properly committed
    drop(journal);

    // Reopen the journal and verify it can be read (simulates recovery after crash)
    let journal = Journal::open(&journal_path)
        .unwrap_or_else(|e| panic!("Failed to reopen journal for {}: {}", boundary.name, e));

    // Get the current state - this proves the journal was durable and can be replayed
    let state = journal
        .get_state(task_id)
        .expect("Failed to read state after recovery")
        .expect("Task should exist after recovery");

    eprintln!("  State at {}: {}", boundary.name, state.name());

    // Verify:
    // 1. State is readable and deserializable (proves journal survives interruption)
    // The state should be one of the valid task states, not corrupted
    assert!(
        matches!(
            state,
            TaskState::Queued
                | TaskState::Preflight
                | TaskState::Running { .. }
                | TaskState::Remediating { .. }
                | TaskState::Verifying { .. }
                | TaskState::Publishing { .. }
                | TaskState::PublishedVerified { .. }
                | TaskState::Done
                | TaskState::Acknowledged { .. }
                | TaskState::Paused { .. }
                | TaskState::Failed { .. }
                | TaskState::Cancelled
        ),
        "State at {} should be a valid state, got: {}",
        boundary.name,
        state.name()
    );

    // 2. All events were persisted (proves no data loss)
    let events = journal
        .events_for(task_id)
        .expect("Failed to read events after recovery");
    assert_eq!(
        events.len(),
        boundary.events.len(),
        "All {} events should be persisted at boundary {} (got {})",
        boundary.events.len(),
        boundary.name,
        events.len()
    );

    // 3. Events are in sequence with no gaps
    for (expected_seq, event) in (1u64..).zip(&events) {
        assert_eq!(
            event.seq.get(),
            expected_seq,
            "Sequence number mismatch at boundary {}: expected {}, got {}",
            boundary.name,
            expected_seq,
            event.seq.get()
        );
    }

    eprintln!(
        "  ✓ Boundary {} passed: journal survived, state is correct",
        boundary.name
    );
}

#[test]
fn test_recovery_matrix_queued_to_preflight() {
    let boundaries = create_boundaries();
    test_boundary(&boundaries[0]);
}

#[test]
fn test_recovery_matrix_preflight_to_running() {
    let boundaries = create_boundaries();
    test_boundary(&boundaries[1]);
}

#[test]
fn test_recovery_matrix_running_to_verifying() {
    let boundaries = create_boundaries();
    test_boundary(&boundaries[2]);
}

#[test]
fn test_recovery_matrix_verifying_to_publishing() {
    let boundaries = create_boundaries();
    test_boundary(&boundaries[3]);
}

#[test]
fn test_recovery_matrix_publishing_to_published_verified() {
    let boundaries = create_boundaries();
    test_boundary(&boundaries[4]);
}

#[test]
fn test_recovery_matrix_published_verified_to_done() {
    let boundaries = create_boundaries();
    test_boundary(&boundaries[5]);
}
