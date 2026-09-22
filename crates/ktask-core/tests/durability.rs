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
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Names the journal path the child appender must open and loop against.
/// Its presence in the environment is what tells this binary, when re-exec'd,
/// that it is running as the child rather than as the top-level test.
const CHILD_ENV: &str = "KTASK_DURABILITY_CHILD_JOURNAL";

/// Not a real test: the entry point
/// [`durability_survives_sigkill_mid_append_at_many_points`] re-execs this
/// binary into, selecting it by exact name, to become the child process that
/// appends events until it is killed. `#[ignore]` keeps it out of the normal
/// test run, since running it directly would loop forever.
#[test]
#[ignore = "invoked directly as a child process by durability_survives_sigkill_mid_append_at_many_points"]
fn child_append_loop() {
    let path = env::var(CHILD_ENV).expect("child: journal path must be set");
    let mut journal = Journal::open(Path::new(&path)).expect("child: open journal");
    let mut n: u64 = 0;
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

#[test]
fn durability_survives_sigkill_mid_append_at_many_points() {
    const TRIALS: u32 = 20;

    let exe = env::current_exe().expect("current test exe path");

    let seed_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .subsec_nanos();
    let mut seed = u64::from(seed_nanos) ^ u64::from(std::process::id());

    let mut total_events_seen: usize = 0;

    for trial in 0..TRIALS {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");

        // Pre-create the schema so the earliest possible kill point tests
        // the journal's durability, not a race in process startup.
        Journal::open(&path).expect("pre-create schema");

        let mut child = Command::new(&exe)
            .args(["child_append_loop", "--exact", "--ignored", "--nocapture"])
            .env(CHILD_ENV, &path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child appender");

        // A short, varying delay: spreads kill points from "before the child
        // opened the journal" through "mid-transaction" to "well into the
        // loop", across the twenty trials.
        let delay_us = next_random(&mut seed) % 50_000;
        std::thread::sleep(Duration::from_micros(delay_us));

        child.kill().expect("SIGKILL the child");
        child.wait().expect("reap the killed child");

        let journal = Journal::open(&path)
            .unwrap_or_else(|e| panic!("trial {trial}: journal must reopen cleanly: {e}"));
        let events = journal
            .events()
            .unwrap_or_else(|e| panic!("trial {trial}: every event must deserialize: {e}"));

        let seqs: Vec<u64> = events.iter().map(|event| event.seq.get()).collect();
        total_events_seen += seqs.len();

        if let Some(&first) = seqs.first() {
            assert_eq!(
                first, 1,
                "trial {trial}: sequence must start at 1, got {seqs:?}"
            );
        }
        for pair in seqs.windows(2) {
            assert_eq!(
                pair[1],
                pair[0] + 1,
                "trial {trial}: sequence numbers must have no gaps or repeats, got {seqs:?}"
            );
        }
    }

    assert!(
        total_events_seen > 0,
        "no trial recorded any event across {TRIALS} trials; the kill delay is too short \
         to prove anything, or the child never ran"
    );
}
