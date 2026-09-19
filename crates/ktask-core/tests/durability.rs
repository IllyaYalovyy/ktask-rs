//! Integration test for journal durability under interruption.
//!
//! Tests that the journal survives a process killed mid-append by:
//! 1. Spawning a child process that appends events in a loop
//! 2. Killing it with SIGKILL at random points
//! 3. Reopening the journal and verifying:
//!    - It opens cleanly
//!    - Every event deserializes
//!    - Sequence numbers have no gaps or repeats

use ktask_core::{EventKind, Journal, TaskId};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::fs;
use std::io::Write;
use std::process::Command;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

const NUM_KILL_POINTS: usize = 20;

/// Generate source code for a helper that appends to the journal.
fn generate_child_source(journal_path: &std::path::Path) -> String {
    format!(
        r#"
use ktask_core::{{Journal, TaskId, EventKind}};

fn main() {{
    let path = std::path::Path::new("{}");
    let mut journal = Journal::open(path).expect("Failed to open journal");

    for i in 0..500 {{
        let task_id = TaskId::new((i % 10) + 1);
        let kind = EventKind::TaskQueued {{
            title: format!("Task {{}}", i),
        }};

        // Append the event. This will be interrupted at various points.
        journal.append(Some(task_id), &kind).expect("Failed to append");

        // Small delay to increase window for interruption
        std::thread::sleep(std::time::Duration::from_millis(2));
    }}
}}
"#,
        journal_path.to_string_lossy()
    )
}

#[test]
fn test_durability_under_interruption() {
    // Build a helper binary first
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let crate_root = std::path::Path::new(manifest_dir).parent().unwrap();
    let workspace_root = crate_root.parent().unwrap();

    for attempt in 0..NUM_KILL_POINTS {
        // Create a fresh temp directory for each iteration
        let temp = TempDir::new().expect("Failed to create temp directory");
        let journal_path = temp.path().join("journal.db");
        let helper_src = temp.path().join("helper.rs");

        // Write the helper source
        let source = generate_child_source(&journal_path);
        let mut f = fs::File::create(&helper_src).expect("Failed to create helper source");
        f.write_all(source.as_bytes())
            .expect("Failed to write helper source");

        // Compile the helper
        let helper_bin = temp.path().join("helper");
        let output = Command::new("rustc")
            .arg("--edition")
            .arg("2021")
            .arg("-L")
            .arg(format!(
                "{}",
                workspace_root.join("target/debug/deps").display()
            ))
            .arg("--extern")
            .arg(format!(
                "ktask_core={}",
                workspace_root
                    .join("target/debug/libktask_core.rlib")
                    .display()
            ))
            .arg("-o")
            .arg(&helper_bin)
            .arg(&helper_src)
            .output()
            .expect("Failed to spawn rustc");

        assert!(
            output.status.success(),
            "Failed to compile helper:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Spawn the child process. We intentionally don't wait() it because
        // we kill it mid-execution to test durability. Zombies are expected here.
        #[allow(clippy::zombie_processes)]
        let child = Command::new(&helper_bin)
            .spawn()
            .expect("Failed to spawn child process");
        let pid = child.id();

        // Let it run for a varying duration based on the attempt
        // This varies the kill point throughout the test
        let delay_ms = 20 + (attempt as u64 * 3) % 80;
        thread::sleep(Duration::from_millis(delay_ms));

        // Kill it with SIGKILL
        let pid_nix = Pid::from_raw(pid.cast_signed());
        let _ = kill(pid_nix, Signal::SIGKILL);

        // Give it time to be killed and for the OS to clean up
        thread::sleep(Duration::from_millis(50));

        // Reopen the journal and verify it's clean
        let journal = Journal::open(&journal_path).unwrap_or_else(|e| {
            panic!("Attempt {attempt}: Failed to reopen journal after SIGKILL: {e}");
        });

        // Read all events and verify they deserialize
        let events = journal.events().unwrap_or_else(|e| {
            panic!("Attempt {attempt}: Failed to read events: {e}");
        });

        // Verify sequence numbers are strictly increasing with no gaps or repeats
        if !events.is_empty() {
            let mut prev_seq = 0u64;
            for (i, event) in events.iter().enumerate() {
                let seq = event.seq.get();

                // Sequence should start at 1
                if i == 0 {
                    assert_eq!(
                        seq, 1,
                        "Attempt {attempt}: First sequence should be 1, got {seq}"
                    );
                }

                // Each sequence should be exactly 1 more than the previous
                assert_eq!(
                    seq,
                    prev_seq + 1,
                    "Attempt {attempt}: Gap detected at index {i}: expected {}, got {seq}",
                    prev_seq + 1
                );

                prev_seq = seq;
            }
        }
    }
}

#[test]
fn test_journal_basic_append() {
    let temp = TempDir::new().expect("Failed to create temp directory");
    let journal_path = temp.path().join("journal.db");

    {
        let mut journal = Journal::open(&journal_path).expect("Failed to open journal");

        // Append a few events
        for i in 0..5 {
            let task_id = TaskId::new(i + 1);
            let kind = EventKind::TaskQueued {
                title: format!("Test task {i}"),
            };
            journal
                .append(Some(task_id), &kind)
                .expect("Failed to append");
        }
    }

    // Reopen and verify
    let journal = Journal::open(&journal_path).expect("Failed to reopen journal");
    let events = journal.events().expect("Failed to read events");

    assert_eq!(events.len(), 5, "Expected 5 events");

    for (i, event) in events.iter().enumerate() {
        assert_eq!(
            event.seq.get(),
            (i + 1) as u64,
            "Sequence number mismatch at index {i}"
        );
    }
}
