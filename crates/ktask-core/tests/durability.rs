//! Durability under interruption: a journal has to survive the death of the
//! process that was writing it (VISION.md section 6, `docs/TESTING.md` Crash
//! recovery).
//!
//! Every transition is journaled before its side effect, so the whole
//! recoverability argument rests on one property: a journal that a process was
//! killed while appending still opens, still reads, and still means what it
//! meant before the kill. That property cannot be tested by handing a function
//! a buffer — the failure lives in the write-ahead log, in a commit frame
//! written half-way and in the `-wal`/`-shm` pair a dead process leaves behind.
//! So this file does the real thing: it starts a second process that appends in
//! a loop, sends it `SIGKILL` — the one signal no process can catch, clean up
//! after, or lose — and then reopens the file the way the next supervisor
//! would.
//!
//! # What a reopened journal has to answer for
//!
//! - it opens at all: WAL recovery walks the log back to the last commit frame
//!   whose checksum holds and refuses the torn tail, rather than the file;
//! - every record it returns deserializes, and each one carries the payload its
//!   position in the loop called for — an event is one transaction, so a kill
//!   cannot leave half a payload behind, and cannot leave a whole one whose
//!   fields arrived scrambled;
//! - the surviving sequences run 1, 2, 3 … with no gap and no repeat: a prefix
//!   of what was written, never a shuffling or a splicing of it;
//! - the sequence is not re-used afterwards either. `AUTOINCREMENT` spends a
//!   number inside the transaction that died with the child, and a journal that
//!   handed that number out a second time would let two events share one record
//!   in every replay of the run.
//!
//! # Why the parent predicts the contents rather than asking the child
//!
//! [`loop_record`] is the loop, and both processes take their expectation from
//! it: the child writes what the function says, and the parent compares what it
//! reads back against the same function. Nothing the child claims about how far
//! it got is believed — the child is the process that was killed. It reports one
//! thing only: that its first commit landed, which is what keeps a kill from
//! racing a journal that held nothing durable yet.
//!
//! # The kill points
//!
//! [`durability_after_sigkill_mid_append`] works through [`KILL_POINTS`] kill
//! points, each a different delay after the child's first commit, so `SIGKILL`
//! lands somewhere else in the append loop every time: before a second commit,
//! inside one, between two. The delays are stratified across the window and
//! jittered from a seed, so the set differs from run to run without clustering;
//! a failure names its seed, and `KTASK_DURABILITY_SEED` replays it.

use std::collections::BTreeSet;
use std::io::Read as _;
use std::io::Write as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ktask_core::{AttemptId, Event, EventKind, EventSeq, Journal, TaskId, journal_path};
use tempfile::{TempDir, tempdir};

/// `SIGKILL`, which is what [`Child::kill`] sends on Unix and the one signal an
/// appending process cannot catch, flush, or argue with.
const SIGKILL: i32 = 9;

/// How many kill points the durability test works through. A mid-append kill is
/// the phase boundary inside a single record, and VISION.md section 6 wants each
/// boundary killed rather than reasoned about.
const KILL_POINTS: u32 = 24;

/// The widest window a kill point waits after the child's first commit, in
/// microseconds. The child appends as fast as one `fsync` per record allows, so
/// waiting longer would only test a journal that had stopped being written.
const LONGEST_KILL_MICROS: u64 = 60_000;

/// How many records the un-killed child role writes before it is finished.
const WHOLE_LOOP: u32 = 64;

/// The most records the child will write if the parent never kills it. No kill
/// point comes near this: it is the stop for the orphan a failing test might
/// otherwise leave appending into a deleted directory, not a case on purpose.
const APPEND_CAP: u32 = 50_000;

/// How long a kill point waits for the child's first commit before giving up.
/// Generous, because a contended machine can take seconds to start a process.
const FIRST_COMMIT_WAIT: Duration = Duration::from_secs(30);

/// How often that wait looks for the signal file. Fine on purpose: the delay
/// before a kill has to be counted from the child's first commit, not from
/// whenever a coarse poll happens to notice it, and a poll every few hundred
/// microseconds is what puts `SIGKILL` inside the append loop rather than a
/// quarter of a second into it.
const SIGNAL_POLL: Duration = Duration::from_micros(200);

/// The file a child touches once its first record is committed.
const SIGNAL_FILE: &str = "first-commit";

/// The environment that turns another copy of this test binary into the
/// appending child: which role, which journal, which signal file.
const CHILD_ROLE: &str = "KTASK_DURABILITY_CHILD";
const CHILD_JOURNAL: &str = "KTASK_DURABILITY_JOURNAL";
const CHILD_SIGNAL: &str = "KTASK_DURABILITY_SIGNAL";

/// The one test the child is told to run, by name and exactly.
const CHILD_TEST: &str = "durability_child_appends_the_loop_the_parent_predicts";

/// Replays one schedule: `KTASK_DURABILITY_SEED` takes the number a failure
/// message printed.
const KILL_SEED: &str = "KTASK_DURABILITY_SEED";

/// One line of the wide payload every fifth record carries. Repeated 128 times
/// it is about eight kilobytes, or two or three pages of `events`: a commit that
/// size is written over several writes, so a `SIGKILL` has a real chance of
/// arriving in the middle of one and leaving a tail recovery has to refuse while
/// every committed record beside it survives.
const WIDE_DETAIL: &str = "a torn tail of a write-ahead log is refused, not replayed\n";

/// What the parent told a copy of itself to do: which journal to append to, and
/// where to say that the first record landed.
struct Job {
    journal: PathBuf,
    signal: PathBuf,
}

/// The child role assigned to this process, if the parent assigned one.
fn job_from_environment() -> Option<Job> {
    if std::env::var(CHILD_ROLE).ok().as_deref() != Some("1") {
        return None;
    }
    let journal = std::env::var_os(CHILD_JOURNAL).map(PathBuf::from)?;
    let signal = std::env::var_os(CHILD_SIGNAL).map(PathBuf::from)?;
    Some(Job { journal, signal })
}

/// The record the append loop writes at position `index`, and the task it
/// belongs to.
///
/// Both processes build their expectation of the journal from this one
/// function, which is what lets the parent say "what survived is exactly a
/// prefix of what was written" about the file rather than about the dead
/// child's word for it. The five shapes rotate so a recovered journal cannot be
/// a shuffling of the loop, the two `None` tasks in every three records keep a
/// queue-level event from being confused with a task's own, and the wide payload
/// in every fifth record makes one commit span several pages of the
/// write-ahead log — which is what gives a kill a window to tear a commit at
/// all.
fn loop_record(index: u32) -> (Option<TaskId>, EventKind) {
    let task = match index % 3 {
        0 => None,
        position => Some(TaskId::new(position)),
    };
    let kind = match index % 5 {
        0 => EventKind::TaskQueued {
            title: format!("kill-point event {index}"),
        },
        1 => EventKind::PreflightStarted,
        2 => EventKind::AttemptStarted {
            attempt: AttemptId::new(index),
            protocol: "tdd".to_owned(),
            pid: index,
            base_sha: format!("{index:040}"),
        },
        3 => EventKind::TaskDone {
            commit: format!("commit {index:040}"),
        },
        _ => EventKind::TaskCancelled {
            reason: WIDE_DETAIL.repeat(128),
        },
    };
    (task, kind)
}

/// The first way in which `events` is *not* the first `written` records of the
/// append loop, or `None` when it is exactly that.
///
/// Three separate claims are checked here, because a crash can break any one of
/// them alone: the sequence has to run 1, 2, 3 … (a gap is a lost record, a
/// repeat is two records claiming one sequence), every record has to carry the
/// payload its own position calls for (a torn or reordered row decodes into
/// something else), and the journal cannot hold more than was written.
fn loop_prefix_problem(events: &[Event], written: u32) -> Option<String> {
    let allowed = usize::try_from(written).unwrap_or(usize::MAX);
    if events.len() > allowed {
        return Some(format!(
            "the journal holds {} records, which is more than the {written} the loop wrote",
            events.len()
        ));
    }
    for (offset, record) in events.iter().enumerate() {
        let Ok(index) = u32::try_from(offset) else {
            return Some(format!(
                "the journal holds {offset} records, which is past the last position the loop can number"
            ));
        };
        let expected = EventSeq::new(u64::from(index) + 1);
        if record.seq != expected {
            return Some(format!(
                "record number {} is numbered {seq}, so the journal's sequence has a gap or a repeat",
                offset + 1,
                seq = record.seq
            ));
        }
        let (task, kind) = loop_record(index);
        if record.task_id != task || record.kind != kind {
            return Some(format!(
                "the record numbered {expected} is {actual:?} about {actual_task:?}, not the \
                 {kind:?} about {task:?} that the loop writes at position {index}",
                actual = record.kind,
                actual_task = record.task_id
            ));
        }
    }
    None
}

/// Write the loop's records into `journal`, one transaction each, until `cap` of
/// them are committed — or, with `signal` given, until the process running this
/// is killed.
///
/// `signal` is touched once, after the first record is committed and durable.
/// Without it a kill could land before the child had written anything, and an
/// empty journal afterwards would be about process timing rather than about
/// durability.
fn append_loop(journal: &Path, signal: Option<&Path>, cap: u32) -> Result<u32, String> {
    let mut journal = Journal::open(journal).map_err(|problem| {
        format!(
            "the appending process could not open `{}`: {problem}",
            journal.display()
        )
    })?;
    for index in 0..cap {
        let (task, kind) = loop_record(index);
        journal.append(task, &kind).map_err(|problem| {
            format!("the appending process could not commit record {index}: {problem}")
        })?;
        if index == 0
            && let Some(path) = signal
        {
            std::fs::write(path, "committed\n").map_err(|problem| {
                format!(
                    "the first committed record could not be signalled at `{}`: {problem}",
                    path.display()
                )
            })?;
        }
    }
    Ok(cap)
}

/// The schema version a journal written by this build reports — read from a
/// journal of its own, rather than repeated from a constant this file could not
/// keep honest.
fn this_build_schema_version() -> Result<i64, String> {
    let directory = scratch_directory()?;
    let fresh = Journal::open(&journal_path(directory.path()))
        .map_err(|problem| format!("a fresh journal would not open: {problem}"))?;
    fresh
        .schema_version()
        .map_err(|problem| format!("a fresh journal could not say its schema version: {problem}"))
}

/// Reopen the journal one kill point left behind and report what survived.
///
/// Returns the number of records the kill left, having checked that they are the
/// loop's prefix, that the schema version row survived with them, and that the
/// next append continues the sequence rather than re-using a number the
/// interrupted transaction had already spent.
///
/// A fresh [`Journal`] rather than the dead child's connection is the point: the
/// question is what the file holds, and a connection answers only for the
/// process that owns it.
fn recover(journal: &Path, expected_version: i64) -> Result<u32, String> {
    let mut reopened = Journal::open(journal)
        .map_err(|problem| format!("the journal did not reopen after SIGKILL: {problem}"))?;
    let version = reopened.schema_version().map_err(|problem| {
        format!("the reopened journal could not say its schema version: {problem}")
    })?;
    if version != expected_version {
        return Err(format!(
            "the reopened journal is on schema version {version}, not the {expected_version} this \
             build writes: the kill took the `meta` row with it"
        ));
    }
    let events = reopened.events().map_err(|problem| {
        format!("the reopened journal holds a record that will not deserialize: {problem}")
    })?;
    let preserved = u32::try_from(events.len()).map_err(|problem| {
        format!(
            "{} records is not a count the loop can index: {problem}",
            events.len()
        )
    })?;
    if preserved == 0 {
        return Err(
            "nothing survived the kill, and the child had committed a record before SIGKILL was sent"
                .to_owned(),
        );
    }
    if let Some(problem) = loop_prefix_problem(&events, preserved) {
        return Err(problem);
    }
    let (task, kind) = loop_record(preserved);
    let continued = reopened
        .append(task, &kind)
        .map_err(|problem| format!("the reopened journal refused the next append: {problem}"))?;
    let expected = EventSeq::new(u64::from(preserved) + 1);
    if continued != expected {
        return Err(format!(
            "the reopened journal numbered its next record {continued} instead of {expected}, so a \
             sequence the interrupted commit had spent was handed out twice"
        ));
    }
    let whole = reopened.events().map_err(|problem| {
        format!("the record appended after the kill cannot be read back: {problem}")
    })?;
    if let Some(problem) = loop_prefix_problem(&whole, preserved + 1) {
        return Err(problem);
    }
    Ok(preserved)
}

/// A scratch directory to write a journal in: `docs/DESIGN.md` Conventions keeps
/// a test out of the repository.
fn scratch_directory() -> Result<TempDir, String> {
    tempdir().map_err(|problem| format!("no scratch directory to write a journal in: {problem}"))
}

/// Start the appending child: another copy of this test binary, told by the
/// environment which journal is its business.
///
/// Its standard error comes back to the parent rather than to the terminal,
/// because the child is about to be killed and a panic or a refusal inside it is
/// the parent's diagnosis, not the child's report.
fn spawn_child(
    directory: &Path,
    journal: &Path,
    signal: &Path,
) -> Result<(Child, Option<ChildStderr>), String> {
    let executable = std::env::current_exe().map_err(|problem| {
        format!("the appending child is another copy of this binary, and this binary cannot name itself: {problem}")
    })?;
    let mut child = Command::new(executable)
        .env(CHILD_ROLE, "1")
        .env(CHILD_JOURNAL, journal)
        .env(CHILD_SIGNAL, signal)
        // `cargo nextest` names a protocol descriptor on the environment; a
        // grandchild answering on its parent's protocol stream would corrupt
        // the report of the very test that spawned it.
        .env_remove("NEXTEST_TEST_BUFFER_ID")
        // Coverage runs say where the counts go. In the scratch directory, not
        // in the repository and not on top of the parent's file.
        .env_remove("LLVM_PROFILE_FILE")
        .current_dir(directory)
        // Exactly one test — the child role — and no capture standing between a
        // failure inside it and the pipe the parent reads.
        .args(["--exact", CHILD_TEST, "--nocapture"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|problem| format!("the appending child could not be started: {problem}"))?;
    let output = child.stderr.take();
    Ok((child, output))
}

/// Wait until the child has said that its first record is committed.
fn wait_for_first_commit(signal: &Path) -> Result<(), String> {
    let deadline = Instant::now() + FIRST_COMMIT_WAIT;
    loop {
        if signal.try_exists().map_err(|problem| {
            format!("`{}` could not be looked for: {problem}", signal.display())
        })? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("no first commit within {FIRST_COMMIT_WAIT:?}"));
        }
        std::thread::sleep(SIGNAL_POLL);
    }
}

/// What a finished child wrote to its standard error, if it wrote anywhere.
fn read_stderr(output: Option<&mut ChildStderr>) -> String {
    let mut words = String::new();
    if let Some(pipe) = output
        && pipe.read_to_string(&mut words).is_err()
    {
        // These words diagnose a failure; they are never the failure.
        words.clear();
    }
    words
}

/// `SIGKILL` the appending child, wait for it to be reaped, and hand back
/// whatever it managed to say.
///
/// Waiting is not politeness: the parent opens the same file a moment later, and
/// a journal with a live writer would answer a durability question with a
/// locking one.
fn stop(child: &mut Child, output: Option<&mut ChildStderr>) -> Result<String, String> {
    child.kill().map_err(|problem| {
        format!("SIGKILL could not be delivered to the appending child: {problem}")
    })?;
    let status = child.wait().map_err(|problem| {
        format!("the killed appending child could not be waited for: {problem}")
    })?;
    let words = read_stderr(output);
    let signal = status.signal();
    if signal == Some(SIGKILL) {
        return Ok(words);
    }
    Err(format!(
        "the appending child was supposed to die of SIGKILL ({SIGKILL}) and did not: exit code {:?}, \
         signal {signal:?}, saying: {words}",
        status.code()
    ))
}

/// One kill point: a child appending, `SIGKILL` after `delay`, and the journal
/// reopened by this process afterwards.
fn one_kill_point(delay: Duration, expected_version: i64) -> Result<u32, String> {
    let directory = scratch_directory()?;
    let journal = journal_path(directory.path());
    let signal = directory.path().join(SIGNAL_FILE);
    let (mut child, mut output) = spawn_child(directory.path(), &journal, &signal)?;

    // Every way out of here goes through the kill: an orphan would keep
    // appending into a directory this function is about to delete.
    let waited = wait_for_first_commit(&signal).err();
    if waited.is_none() {
        std::thread::sleep(delay);
    }
    let words = stop(&mut child, output.as_mut())?;
    if let Some(problem) = waited {
        return Err(format!(
            "the appending child committed nothing before it was stopped: {problem}. it said: {words}"
        ));
    }
    recover(&journal, expected_version)
}

/// A seed for the kill schedule, different every run, so that one fixed set of
/// kill points cannot quietly become the only set ever tested.
fn kill_seed() -> u64 {
    let pinned = std::env::var(KILL_SEED)
        .ok()
        .and_then(|text| text.parse::<u64>().ok());
    if let Some(pinned) = pinned {
        return pinned;
    }
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|before_the_epoch| {
            Duration::from_secs(before_the_epoch.duration().as_secs())
        });
    since.as_secs().rotate_left(21)
        ^ u64::from(std::process::id())
        ^ u64::from(since.subsec_nanos())
}

/// `splitmix64`: one round of a standard integer mix, kept here rather than
/// buying a random-number crate for one number per kill point.
fn splitmix64(state: u64) -> u64 {
    let stepped = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let spread = (stepped ^ (stepped >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let spread = (spread ^ (spread >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    spread ^ (spread >> 31)
}

/// How long kill point `point` waits after the child's first commit.
///
/// Stratified rather than drawn uniformly: `point` walks the window from nothing
/// to [`LONGEST_KILL_MICROS`] in [`KILL_POINTS`] steps and the seed places the
/// kill at a pseudo-random spot inside its own step. Purely uniform draws
/// cluster, and twenty kills inside the same millisecond are one kill point
/// repeated twenty times.
fn kill_delay(seed: u64, point: u32) -> Duration {
    let steps = u64::from(KILL_POINTS).max(1);
    let step = (LONGEST_KILL_MICROS / steps).max(1);
    let inside = splitmix64(seed ^ u64::from(point)) % step;
    Duration::from_micros(step * u64::from(point) + inside)
}

/// Say something on standard error for the parent to read back.
///
/// Failing here is ignored because there is no one left to tell: this is a
/// diagnosis of a failure that is already being reported.
fn tell(problem: &str) {
    if std::io::stderr().write_all(problem.as_bytes()).is_err() {
        // Nothing usable to say the failure to; the parent reports the kill it
        // was owed instead.
    }
}

/// Every kill point: start a process appending, `SIGKILL` it somewhere inside
/// the loop, and reopen the journal it left.
///
/// It opens cleanly, every record deserializes with the payload its position
/// calls for, the sequences run 1, 2, 3 … with no gap and no repeat, and the
/// next append is numbered after the last survivor rather than over one of them.
#[test]
fn durability_after_sigkill_mid_append() {
    assert!(
        job_from_environment().is_none(),
        "the appending child role must be run alone, with an exact filter naming only it"
    );
    let version = this_build_schema_version()
        .expect("a journal written by this build says which schema version it wrote");
    let seed = kill_seed();
    let mut preserved: Vec<u32> = Vec::new();
    for point in 0..KILL_POINTS {
        let delay = kill_delay(seed, point);
        match one_kill_point(delay, version) {
            Ok(count) => preserved.push(count),
            Err(problem) => panic!(
                "kill point {point} of {KILL_POINTS} sent SIGKILL {delay:?} after the child's first \
                 commit, and its journal did not come back: {problem} \
                 (this schedule is replayable with {KILL_SEED}={seed})"
            ),
        }
    }
    assert!(
        preserved.iter().collect::<BTreeSet<_>>().len() > 1,
        "all {KILL_POINTS} kill points preserved the same number of records, so the kills landed in \
         one place in the append loop rather than at {KILL_POINTS} different points (seed {seed}, \
         preserved {preserved:?})"
    );
}

/// The appending child role, and the un-killed case the kill points are measured
/// against.
///
/// Run by the parent it appends until it is killed, which is why the role is
/// reached through the environment rather than through an argument the harness
/// would have to understand. Run normally it writes the whole loop and exits,
/// and the test asserts what the loop owes anybody: every record it claims
/// reaches the file, in the order it was written, with nothing missing between
/// the first and the last. That is the baseline a recovered prefix is compared
/// against, and it is the proof that [`loop_record`] describes the loop the
/// child actually runs.
#[test]
fn durability_child_appends_the_loop_the_parent_predicts() {
    if let Some(job) = job_from_environment() {
        // The child never returns to a harness: the parent is expected to kill
        // it. Getting here means the parent let it run out, or that appending
        // was refused, and either way the parent's next assertion is about a
        // SIGKILL it never delivered — so say what happened where it can read
        // it.
        match append_loop(&job.journal, Some(&job.signal), APPEND_CAP) {
            Ok(appended) => tell(&format!(
                "the append loop ran to its cap of {appended} records; no SIGKILL ever arrived"
            )),
            Err(problem) => tell(&problem),
        }
        std::process::exit(1);
    }

    let directory = scratch_directory().expect("a scratch directory to write the whole loop in");
    let journal = journal_path(directory.path());
    let appended = append_loop(&journal, None, WHOLE_LOOP)
        .expect("the append loop commits every record it claims to have written");
    assert_eq!(
        appended, WHOLE_LOOP,
        "the append loop stopped short of the records it was asked for"
    );

    // Read through a connection of the reader's own: the process that wrote the
    // loop is gone, and a connection answers only for the one that owns it.
    let reopened = Journal::open(&journal)
        .expect("a journal the loop finished writing opens as cleanly as a fresh one");
    let events = reopened
        .events()
        .expect("every record of a finished loop reads back as the event it was written as");
    let whole = usize::try_from(WHOLE_LOOP).expect("64 records is a count a usize can hold");
    assert_eq!(
        events.len(),
        whole,
        "the loop wrote {WHOLE_LOOP} records and the journal handed back {}",
        events.len()
    );
    if let Some(problem) = loop_prefix_problem(&events, appended) {
        panic!("the finished loop is not the journal it was predicted to be: {problem}");
    }
}
