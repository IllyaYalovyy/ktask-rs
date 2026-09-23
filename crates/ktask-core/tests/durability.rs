//! A journal survives a process killed with `SIGKILL` mid-append.
//!
//! This is VISION.md section 6's crash-recovery guarantee exercised for
//! real, matching `docs/TESTING.md`'s "Crash recovery" layer: the process is
//! actually interrupted, not merely reasoned about. The test re-execs its
//! own binary as a child that appends events in a tight loop, kills it with
//! `SIGKILL` at many different, essentially random points, and then reopens
//! the journal to check it is exactly as durable as [`Journal::append`]
//! promises: it opens cleanly, every stored event still deserializes, and
//! sequence numbers have no gaps or repeats.

use ktask_core::{EventKind, Journal};
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Names the journal path the child appender must open and loop against.
/// Its presence in the environment is what tells this binary, when re-exec'd,
/// that it is running as the child rather than as the top-level test.
const CHILD_ENV: &str = "KTASK_DURABILITY_CHILD_JOURNAL";

/// How long the parent will wait for a marker file before giving up. Marker
/// files appear within microseconds in practice; this is only a backstop
/// against the child never starting at all.
const MARKER_TIMEOUT: Duration = Duration::from_secs(10);

/// Not a real test: the entry point
/// [`durability_survives_sigkill_mid_append_at_many_points`] re-execs this
/// binary into, selecting it by exact name, to become the child process that
/// appends events until it is killed. `#[ignore]` keeps it out of the normal
/// test run, since running it directly would loop forever.
#[test]
#[ignore = "invoked directly as a child process by durability_survives_sigkill_mid_append_at_many_points"]
fn child_append_loop() {
    let path = env::var(CHILD_ENV).expect("child: journal path must be set");
    let journal_path = Path::new(&path);
    let mut journal = Journal::open(journal_path).expect("child: open journal");

    // Signal readiness via a marker *file*, not stdout: this binary is
    // itself a test harness, and libtest writes its own "running 1 test"
    // banner to stdout before invoking this function, which would otherwise
    // be mistaken for our own handshake. A sentinel file is unambiguous and
    // lets the parent's kill-delay clock start once this process is
    // actually running the append loop, not somewhere in exec or
    // dynamic-linker startup, which a busy machine can otherwise stretch
    // long enough to starve every trial.
    let ready_marker = marker_path(journal_path, "ready");
    std::fs::write(&ready_marker, []).expect("child: write ready marker");

    let mut n: u64 = 0;
    let mut acked_first_append = false;
    loop {
        journal
            .append(
                None,
                &EventKind::TaskCancelled {
                    reason: n.to_string(),
                },
            )
            .expect("child: append");
        n += 1;
        // Mark the first *committed* append so the parent can, for one
        // dedicated trial, synchronize on "at least one event is durable"
        // without guessing a wall-clock delay.
        if !acked_first_append {
            let acked_marker = marker_path(journal_path, "acked");
            std::fs::write(&acked_marker, []).expect("child: write acked marker");
            acked_first_append = true;
        }
    }
}

/// The marker file path for `kind` ("ready" or "acked") next to `journal_path`.
fn marker_path(journal_path: &Path, kind: &str) -> PathBuf {
    journal_path.with_extension(format!("{kind}.marker"))
}

/// Blocks until `marker` exists, polling at a fine grain since the wait is
/// normally microseconds; panics if the child never gets there at all.
fn wait_for_marker(marker: &Path) {
    let start = Instant::now();
    while !marker.exists() {
        assert!(
            start.elapsed() < MARKER_TIMEOUT,
            "timed out waiting for marker file {}; the child never reached this point",
            marker.display()
        );
        std::thread::sleep(Duration::from_micros(50));
    }
}

/// A small splitmix64 step, enough to vary the kill delay across trials
/// without a `rand` dependency for one integration test.
fn next_random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Spawns the child appender and waits for its readiness marker. Returns it
/// still running along with the journal path it is looping against.
///
/// Propagates every failure via [`ktask_core::Result`] instead of panicking,
/// so callers keep their `.expect()` calls at the `#[test]` call site, where
/// `clippy.toml`'s `allow-expect-in-tests` applies (it looks at the
/// enclosing function's own attributes, not a helper's).
///
/// # Errors
///
/// Returns [`ktask_core::Error::Database`] if the pre-created schema can't
/// be opened, or [`ktask_core::Error::Io`] if the child can't be spawned.
fn spawn_ready_child(exe: &Path, dir: &tempfile::TempDir) -> ktask_core::Result<(Child, PathBuf)> {
    let path = dir.path().join("journal.db");

    // Pre-create the schema so the earliest possible kill point tests the
    // journal's durability, not a race in process startup.
    Journal::open(&path)?;

    let child = Command::new(exe)
        .args(["child_append_loop", "--exact", "--ignored", "--nocapture"])
        .env(CHILD_ENV, &path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    wait_for_marker(&marker_path(&path, "ready"));

    Ok((child, path))
}

/// Kills `child`, reopens the journal at `path`, and checks the invariants
/// [`Journal::append`] promises regardless of exactly when the kill landed:
/// it opens cleanly, every event deserializes, and sequence numbers have no
/// gaps or repeats. Returns the recovered sequence numbers.
///
/// Propagates I/O and database failures via [`ktask_core::Result`] rather
/// than panicking, for the same reason as [`spawn_ready_child`]; the
/// sequencing assertions stay as `assert_eq!`, which is not restricted by
/// `clippy::unwrap_used`/`expect_used`/`panic` in the first place.
///
/// # Errors
///
/// Returns [`ktask_core::Error::Io`] if the child can't be killed or reaped,
/// and [`ktask_core::Error::Database`] if the journal can't be reopened or
/// its events can't be read back.
fn kill_and_check_recovery(
    mut child: Child,
    path: &Path,
    trial_label: &str,
) -> ktask_core::Result<Vec<u64>> {
    child.kill()?;
    child.wait()?;

    let journal = Journal::open(path)?;
    let events = journal.events()?;

    let seqs: Vec<u64> = events.iter().map(|event| event.seq.get()).collect();

    if let Some(&first) = seqs.first() {
        assert_eq!(
            first, 1,
            "{trial_label}: sequence must start at 1, got {seqs:?}"
        );
    }
    for pair in seqs.windows(2) {
        if let [a, b] = pair {
            assert_eq!(
                *b,
                *a + 1,
                "{trial_label}: sequence numbers must have no gaps or repeats, got {seqs:?}"
            );
        }
    }

    Ok(seqs)
}

#[test]
fn durability_survives_sigkill_mid_append_at_many_points() {
    const TRIALS: u32 = 20;

    let exe = env::current_exe().expect("current test exe path");

    let seed_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .subsec_nanos();
    let mut seed = u64::from(seed_nanos) ^ u64::from(std::process::id());

    // A dedicated trial that proves the harness can capture events at all:
    // it waits for the child's own marker that an append has already been
    // persisted before killing it, so this assertion cannot flake under
    // machine load the way a wall-clock delay could (a busy box can make
    // the append itself take longer than any short sleep, as observed
    // under parallel stress).
    let dir = tempfile::tempdir().expect("tempdir");
    let (child, path) = spawn_ready_child(&exe, &dir).expect("spawn proof-of-life child");
    wait_for_marker(&marker_path(&path, "acked"));
    let seqs = kill_and_check_recovery(child, &path, "proof-of-life trial")
        .expect("proof-of-life trial: kill and recover");
    assert!(
        !seqs.is_empty(),
        "the child marked a committed append, but the reopened journal has no events"
    );

    // Randomized trials spanning "just after the child opened the journal"
    // through "mid-transaction" to "well into the loop": they may or may
    // not catch an event in flight, which is fine, since what they check is
    // that recovery is well-formed no matter when the kill landed.
    for trial in 0..TRIALS {
        let dir = tempfile::tempdir().expect("tempdir");
        let (child, path) = spawn_ready_child(&exe, &dir).expect("spawn trial child");

        let delay_us = next_random(&mut seed) % 50_000;
        std::thread::sleep(Duration::from_micros(delay_us));

        kill_and_check_recovery(child, &path, &format!("trial {trial}"))
            .expect("trial: kill and recover");
    }
}
