//! The repository lock: one file that says who is publishing.
//!
//! VISION.md §10 step 5 publishes "under a repository lock that serializes all
//! integration and publication operations", and the serialization that matters
//! is across *processes*: two supervisor runs on one machine — a second TUI
//! opened against the same project, a run someone started by hand while the
//! first is wedged — otherwise fetch, merge and push mainline from two
//! directions and interleave them. [`acquire`] is the whole primitive: it takes
//! the lock, waits for it, or refuses with the reason.
//!
//! # A file, not a kernel advisory lock
//!
//! `flock` would need no staleness rule, because the kernel drops it when the
//! holder dies. It was rejected for two reasons specific to this tool. A kernel
//! lock leaves no trace of who held it, so the crash this lock exists to
//! survive leaves nothing to report — and this task's contract is that a
//! reclamation *is reported*, which needs a record of the holder. And an
//! advisory lock is inherited across `fork`, which for a supervisor that runs
//! every gate and every agent as its own process group (ADR-0038) means the
//! lock would be held by every child it ever spawned and would outlive the run
//! that took it. A lock file is legible after a crash, names its holder, and
//! cannot leak into a child. The mechanism is ADR-0047.
//!
//! # The record, and why pid alone is not enough
//!
//! The file holds one JSON object: the holder's `pid`, the tick count at which
//! that process began (`started_ticks`), the id of the boot it was written in
//! (`boot_id`), a per-attempt `token`, and the instant it was written (`since`).
//! A pid is not an identity: the kernel reuses them, and a machine that died
//! mid-publication comes back with fresh pids over a stale file. The start tick
//! tells a reused pid from the process that wrote the file, and the boot id
//! says whether that pid could have survived at all. A record that cannot be
//! read is *not* evidence of an abandoned lock, so `boot_id` and
//! `started_ticks` are optional here and unknown means "keep waiting".
//!
//! # Claiming: a hard link, so a lock file is never half written
//!
//! The record is written to a uniquely named draft file and then hard-linked
//! into place, which is an exclusive create — the link fails with
//! `AlreadyExists` if a lock file is there — that makes the completed record
//! visible in the same step that creates the file. A create-then-write would
//! leave a window in which the lock file exists and says nothing, and closing
//! that window afterwards would need a rule about how old a silent lock file
//! has to be before it may be deleted. Every threshold such a rule could pick
//! is wrong on some machine. The draft is removed on every exit path, so an
//! `acquire` leaves either the lock file or nothing.
//!
//! # What may be reclaimed, and what never is
//!
//! A lock is taken over only when its holder is *provably* gone: the pid is
//! absent from the kernel's table, or it is a zombie that has stopped executing
//! and will never delete anything, or the pid is live but began at a different
//! tick than the one the record named, or the record was written in another
//! boot. Everything else is waited behind, and never deleted: a live holder, a
//! holder owned by a user this process may not signal, a pid too large for this
//! platform to name, and a file whose record cannot be read. Deleting a lock
//! that cannot be proved abandoned is the failure this module must not make: it
//! converts a wait into two processes publishing at once. The reclamation is
//! handed back as [`Reclaimed`] — pid, when it was written, and why it counted
//! as abandoned — for the caller to record and the TUI to show.
//!
//! The one gap this leaves is two processes reclaiming the same abandoned lock
//! at the same instant. The record is re-read and compared immediately before
//! the unlink, which shrinks the window to the microseconds between them but
//! cannot close it. It is closed nowhere here, deliberately: ADR-0046 decided
//! that publication is proven by the tip a fetch brought back, not by the lock
//! that was held while pushing. The lock keeps interference rare; the proof
//! stays elsewhere.
//!
//! # Giving the lock back
//!
//! The `token` is what lets a release prove the file is still the one it
//! created: a lock that was reclaimed behind this holder's back belongs to
//! somebody else, and unlinking it would delete the wrong holder's lock. A
//! release therefore refuses ([`Error::Policy`]) when the file names another
//! holder, has been removed, or has gone unreadable; [`Drop`] runs the same
//! check and stays silent, because a destructor has no one to tell.
//!
//! # Waiting
//!
//! Waiting is a poll on the filesystem rather than a notification: a few dozen
//! bytes, read every 20 ms until the caller's deadline. When the deadline
//! passes, [`acquire`] refuses with [`std::io::ErrorKind::TimedOut`] and a
//! message naming the file, the holder's pid and the instant it wrote the
//! record — the three things that decide whether to keep waiting. Zero timeout
//! means look once and report, which is what a caller that would rather fail
//! than block asks for.
//!
//! The directory is never created: like [`crate::journal`], this module writes
//! into a state directory that registration owns (VISION.md §11), and an
//! absent one is reported rather than conjured at a default mode. The lock file
//! itself is created `0600`.

use std::collections::hash_map::RandomState;
use std::fmt::Display;
use std::fs::{self, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::Write as _;
use std::io::{self, ErrorKind};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use serde::ser::Error as _;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::{Error, Result};

/// The name of the lock file inside the directory being locked.
const LOCK_FILE_NAME: &str = "repo.lock";

/// The prefix of a draft record, written before it can become the lock file.
/// The name says what it is to anyone listing the directory during a wait.
const DRAFT_PREFIX: &str = "repo.lock.pending-";

/// The mode a draft and a lock file are created with. The record names a pid, a
/// boot and a token, and it lives beside a project's private state (VISION.md
/// §11), so it is readable by its owner alone from the first write.
const LOCK_FILE_MODE: u32 = 0o600;

/// How the kernel counts the instant a process began, in `/proc/<pid>/stat`.
///
/// `starttime` is the 22nd field counted from the front of the line; the first
/// two are the pid and the parenthesised command name, and the fields are
/// counted after that closing parenthesis, so it is the 20th of what is left.
const START_TIME_FIELD: usize = 19;

/// How often a waiting `acquire` asks the filesystem again. The same interval a
/// timed-out gate's death is watched at (ADR-0038): short enough that a lock
/// handed back is taken at once, long enough that a long wait is not a spin.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The lock file of one directory.
///
/// The name of the file a run contends on is decided here and nowhere else, the
/// way [`crate::journal::journal_path`] decides its own database name.
#[must_use]
pub fn lock_path(dir: &Path) -> PathBuf {
    dir.join(LOCK_FILE_NAME)
}

/// Take the repository lock of `dir`, waiting up to `timeout` for whoever holds
/// it to give it back.
///
/// `dir` is the directory the operations in VISION.md §10 share — a project's
/// state directory, which `register` created — and not the worktree being
/// published: the lock is what keeps two runs out of each other's integration,
/// and a worktree is private to one task by construction.
///
/// A lock left by a holder that has provably gone is taken over at once and
/// reported by [`RepoLock::reclaimed`]; a lock that cannot be proved abandoned
/// is waited behind until `timeout` passes. A `timeout` of [`Duration::ZERO`]
/// looks once and reports, without sleeping.
///
/// # Errors
///
/// [`Error::NotFound`] when `dir` is not a directory.
/// [`std::io::ErrorKind::TimedOut`] inside [`Error::Io`] when the lock is held
/// for the whole `timeout`: the message names the file, the holder's pid and
/// when that holder wrote it. An [`Error::Io`] from the filesystem itself — a
/// directory that will not accept a draft, a filesystem with no hard links — is
/// reported as it comes, with the path it touched.
pub fn acquire(dir: &Path, timeout: Duration) -> Result<RepoLock> {
    if !dir.is_dir() {
        return Err(Error::NotFound {
            what: format!("the directory to lock, `{}`", dir.display()),
        });
    }
    let started = Instant::now();
    // A timeout no `Instant` can carry — `attempt_timeout_secs` is a `u64` read
    // straight out of the configuration (VISION.md §11) — leaves this caller with
    // no deadline at all, which is what "wait as long as the holder takes" has to
    // mean. Adding the two instead panics, and a panic here is a run that ends
    // with nothing journaled about why it ended.
    let deadline = started.checked_add(timeout);
    let path = lock_path(dir);
    let token = token();
    let draft = Draft::new(dir, &token);
    let mut reclaimed: Option<Reclaimed> = None;

    loop {
        draft.write()?;
        match draft.link_into(&path) {
            // The link succeeded, so the record in the file is this one's.
            Ok(()) => {
                return Ok(RepoLock {
                    path,
                    token,
                    reclaimed,
                });
            }
            Err(existing) if existing.kind() == ErrorKind::AlreadyExists => {}
            Err(refused) => return Err(refused.into()),
        }
        match answer_for(&path)? {
            // The most recent takeover is the one worth reporting.
            Answer::Retry(report) => reclaimed = report.or(reclaimed),
            Answer::Blocked(why) => {
                if deadline.is_some_and(|until| Instant::now() >= until) {
                    return Err(gave_up(&path, &why, started.elapsed()));
                }
                std::thread::sleep(up_to_deadline(deadline));
            }
        }
    }
}

/// A lock file this process holds, given back when this is dropped.
///
/// Holding one is what makes "the candidate was published under the lock" a
/// statement about a fact rather than about good timing, so the type has no
/// constructor that does not create the file.
#[derive(Debug)]
pub struct RepoLock {
    /// The lock file itself.
    path: PathBuf,
    /// The token this holder wrote, which is how a release knows the file is
    /// still the one it made.
    token: String,
    /// The abandoned lock this acquire took over, if it took one over.
    reclaimed: Option<Reclaimed>,
}

impl RepoLock {
    /// The lock file this holder created.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The lock this one replaced, and why replacing it was justified.
    ///
    /// A takeover is never silent: a run that finds a lock left by a process
    /// that no longer exists has to say so in its evidence, because the reason
    /// a lock is left behind is usually the reason the publication before it
    /// did not finish.
    #[must_use]
    pub fn reclaimed(&self) -> Option<&Reclaimed> {
        self.reclaimed.as_ref()
    }

    /// Give the lock back now, rather than when this goes out of scope.
    ///
    /// # Errors
    ///
    /// [`Error::Policy`] when the lock file no longer names this holder —
    /// replaced by another process, removed, or no longer readable — because
    /// removing it then would delete somebody else's lock. A publication that
    /// lost its lock mid-flight is a fact the run has to hear.
    pub fn release(self) -> Result<()> {
        self.give_up()
    }

    /// Remove the lock file, but only while it still names this holder.
    fn give_up(&self) -> Result<()> {
        match read_record(&self.path) {
            Readable::Record(record) if record.token == self.token => {
                fs::remove_file(&self.path)?;
                Ok(())
            }
            holder => Err(Error::Policy {
                detail: match holder {
                    Readable::Record(record) => format!(
                        "the repository lock at `{}` names pid {} as its holder, not this one, \
                         so it is not ours to remove",
                        self.path.display(),
                        record.pid
                    ),
                    Readable::Gone => format!(
                        "the repository lock at `{}` is no longer there: it was removed while \
                         this process believed it held it",
                        self.path.display()
                    ),
                    Readable::Unreadable(reason) => format!(
                        "the repository lock at `{}` no longer holds a record this process can \
                         read: {reason}",
                        self.path.display()
                    ),
                },
                paths: vec![self.path.clone()],
            }),
        }
    }
}

impl Drop for RepoLock {
    /// Give the lock back. The refusal [`RepoLock::release`] would have reported
    /// goes with the destructor's silence: there is no lock left to take by
    /// deleting a file that names somebody else, and a destructor has no caller
    /// to hand a failure to.
    fn drop(&mut self) {
        let _ = self.give_up();
    }
}

/// A lock file this `acquire` took over because its holder had provably gone.
///
/// It is the report of a reclamation: what was found, and the ground for taking
/// it. A run records it (VISION.md §14's flight recorder) rather than
/// swallowing it, because an abandoned lock is evidence about the attempt that
/// left it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaimed {
    /// The lock file that was removed and replaced.
    pub path: PathBuf,
    /// The pid the replaced record named.
    pub pid: u32,
    /// When that holder wrote it, in its own words.
    pub since: String,
    /// Why it counted as abandoned rather than held.
    pub reason: Abandoned,
}

impl Display for Reclaimed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "reclaimed the repository lock at `{}`, left by pid {} and written {}: {}",
            self.path.display(),
            self.pid,
            self.since,
            self.reason
        )
    }
}

/// Why the process a lock record names cannot still be the one holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Abandoned {
    /// The kernel has no such process, or has only an unreaped zombie that will
    /// never delete a file.
    DeadPid,
    /// The pid is live, but a process that began at a different tick owns it:
    /// the kernel reused the number, and the holder died.
    PidReused {
        /// The start tick the record named.
        recorded: u64,
        /// The start tick of the process that owns the pid now.
        now: u64,
    },
    /// The record predates this boot, so nothing it names can be running.
    Rebooted {
        /// The boot the record named.
        recorded: String,
        /// The boot this process is running in.
        now: String,
    },
}

impl Display for Abandoned {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Abandoned::DeadPid => write!(formatter, "the process is not running"),
            Abandoned::PidReused { recorded, now } => write!(
                formatter,
                "the pid belongs now to a process that began {now} ticks after boot, not the one \
                 that began {recorded}"
            ),
            Abandoned::Rebooted { recorded, now } => write!(
                formatter,
                "it was written in boot {recorded} and this is boot {now}, so no process it \
                 names survived"
            ),
        }
    }
}

/// What an existing lock file answered when it was asked whether it is held.
enum Answer {
    /// The file is gone, or was taken over: claim it again, with the report of
    /// the takeover when this caller made one.
    Retry(Option<Reclaimed>),
    /// Something holds it, for this reason.
    Blocked(Why),
}

/// Why an existing lock file cannot be taken over.
enum Why {
    /// A live process wrote it, and is still running.
    Live {
        /// The holder the record named.
        pid: u32,
        /// When it says it wrote the file.
        since: String,
    },
    /// A process with that pid exists, but this one cannot examine it.
    OutOfReach {
        /// The holder the record named.
        pid: u32,
        /// What the machine answered.
        reason: String,
    },
    /// The file is there and its record cannot be read, so its holder is
    /// unknown — and an unknown holder is never presumed dead.
    Unreadable {
        /// What the read or the parse answered.
        reason: String,
    },
}

impl Display for Why {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Why::Live { pid, since } => {
                write!(formatter, "it is held by pid {pid} since {since}")
            }
            Why::OutOfReach { pid, reason } => write!(
                formatter,
                "pid {pid} exists but this process may not examine it: {reason}"
            ),
            Why::Unreadable { reason } => {
                write!(
                    formatter,
                    "the lock file's record could not be read: {reason}"
                )
            }
        }
    }
}

/// Ask an existing lock file whether it is held, taking it over if it is not.
fn answer_for(path: &Path) -> Result<Answer> {
    let found = match read_record(path) {
        Readable::Record(record) => record,
        Readable::Gone => return Ok(Answer::Retry(None)),
        Readable::Unreadable(reason) => return Ok(Answer::Blocked(Why::Unreadable { reason })),
    };
    match lot_of(&found) {
        Lot::Held(why) => Ok(Answer::Blocked(why)),
        Lot::Abandoned(reason) => take_over(path, &found, reason),
    }
}

/// Remove an abandoned lock file, if it is still the one that was found.
///
/// The re-read is the whole race protection this module has: a lock file that
/// changed while the verdict was being reached is somebody else's now, and is
/// left alone rather than deleted on a judgement that has gone stale.
fn take_over(path: &Path, abandoned: &Record, reason: Abandoned) -> Result<Answer> {
    let still_there =
        matches!(read_record(path), Readable::Record(current) if current == *abandoned);
    if !still_there {
        return Ok(Answer::Retry(None));
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(Answer::Retry(Some(Reclaimed {
            path: path.to_path_buf(),
            pid: abandoned.pid,
            since: abandoned.since.clone(),
            reason,
        }))),
        Err(absent) if absent.kind() == ErrorKind::NotFound => Ok(Answer::Retry(None)),
        Err(refused) => Err(refused.into()),
    }
}

/// Whether the process a record names can still be holding the file.
enum Lot {
    /// It is, or cannot be proved otherwise: wait.
    Held(Why),
    /// It cannot be, for this reason: the lock may be taken over.
    Abandoned(Abandoned),
}

/// Decide whether a record's holder is still the process it named.
fn lot_of(record: &Record) -> Lot {
    if let (Some(recorded), Some(this)) = (record.boot_id.as_deref(), boot_id().as_deref())
        && recorded != this
    {
        return Lot::Abandoned(Abandoned::Rebooted {
            recorded: recorded.to_owned(),
            now: this.to_owned(),
        });
    }
    match life_of(record.pid) {
        Life::Gone => Lot::Abandoned(Abandoned::DeadPid),
        Life::Running { started } => match (record.started_ticks, started) {
            (Some(recorded), Some(now)) if recorded != now => {
                Lot::Abandoned(Abandoned::PidReused { recorded, now })
            }
            _ => Lot::Held(Why::Live {
                pid: record.pid,
                since: record.since.clone(),
            }),
        },
        Life::OutOfReach { reason } => Lot::Held(Why::OutOfReach {
            pid: record.pid,
            reason,
        }),
    }
}

/// What the machine says about a pid right now.
enum Life {
    /// The process runs, and when it began wherever the machine can say.
    Running {
        /// The tick count at which it began.
        started: Option<u64>,
    },
    /// The kernel has no such process, or has only an unreaped zombie: a zombie
    /// has stopped executing and will never remove a file.
    Gone,
    /// A process with that pid exists and this one may not ask it anything.
    OutOfReach {
        /// What the machine answered.
        reason: String,
    },
}

/// Ask the kernel whether `pid` exists, and the machine when it began.
///
/// Signal 0 is the question rather than a signal: it reports whether the pid
/// could be signalled without asking anything of the process. `EPERM` is
/// therefore an answer of "yes, and it is not yours", not an answer of "no",
/// and it is treated as one.
fn life_of(pid: u32) -> Life {
    let stat = proc_stat(pid);
    let Ok(as_pid) = i32::try_from(pid) else {
        return Life::OutOfReach {
            reason: format!("{pid} is not a process id this platform can name"),
        };
    };
    match kill(Pid::from_raw(as_pid), None) {
        Ok(()) => match stat {
            Some(('Z', _)) => Life::Gone,
            Some((_, started)) => Life::Running {
                started: Some(started),
            },
            None => Life::Running { started: None },
        },
        Err(Errno::ESRCH) => Life::Gone,
        Err(other) => Life::OutOfReach {
            reason: other.to_string(),
        },
    }
}

/// What a lock file yielded when it was read.
enum Readable {
    /// A record that names a holder.
    Record(Record),
    /// No file: it went away between one step and the next.
    Gone,
    /// A file that does not name a holder this process can believe.
    Unreadable(String),
}

/// Read a lock file's record, refusing to guess at one it cannot read.
fn read_record(path: &Path) -> Readable {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(absent) if absent.kind() == ErrorKind::NotFound => return Readable::Gone,
        Err(refused) => return Readable::Unreadable(refused.to_string()),
    };
    let record = match serde_json::from_str::<Record>(&text) {
        Ok(record) => record,
        Err(malformed) => return Readable::Unreadable(malformed.to_string()),
    };
    if record.pid == 0 {
        // Signalling pid 0 means every process in this process group, which for
        // a supervisor is every gate and agent child it has ever started. A
        // record that names 0 names no holder, and must never be probed.
        return Readable::Unreadable(
            "no process has pid 0, so the record names no holder".to_owned(),
        );
    }
    Readable::Record(record)
}

/// The contents of a lock file: who wrote it, when, and to what purpose.
///
/// Unknown fields are tolerated rather than refused: a record written by a
/// later ktask-rs still names its holder to this one, and a lock file this
/// build cannot fully read is one it will wait behind forever. A field is
/// `Option` where the machine may not answer, because "unknown" means "keep
/// waiting" and never "the holder is gone".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    /// The process that wrote the file.
    pid: u32,
    /// The tick count at which that process began, when `/proc` could say.
    #[serde(default)]
    started_ticks: Option<u64>,
    /// The boot the file was written in, when the machine could say.
    #[serde(default)]
    boot_id: Option<String>,
    /// This holder's own identifier, which survives to the release.
    token: String,
    /// The instant the record was written, RFC 3339.
    since: String,
}

impl Record {
    /// A record for this process, dated now, naming `token` as its holder.
    fn written_now(token: &str) -> Result<Self> {
        let pid = std::process::id();
        Ok(Self {
            pid,
            started_ticks: process_start_ticks(pid),
            boot_id: boot_id(),
            token: token.to_owned(),
            since: written_now()?,
        })
    }
}

/// The record being offered for the lock, written before it can become one.
///
/// It is dated again on every attempt, so the instant a holder claims to have
/// written the lock is within one poll of the moment it actually took the lock.
struct Draft {
    /// The uniquely named file the record is written to.
    path: PathBuf,
    /// The token this acquire will hold if the link succeeds.
    token: String,
}

impl Draft {
    /// The draft file of one acquire, which no other process can name.
    fn new(dir: &Path, token: &str) -> Self {
        Self {
            path: dir.join(format!("{DRAFT_PREFIX}{token}")),
            token: token.to_owned(),
        }
    }

    /// Date the record now and write the draft, ready to be linked into place.
    fn write(&self) -> Result<()> {
        let record = Record::written_now(&self.token)?;
        let text = serde_json::to_vec(&record)?;
        let mut draft = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(LOCK_FILE_MODE)
            .open(&self.path)?;
        draft.write_all(&text)?;
        // The record exists to outlive the process that wrote it, so it reaches
        // the disk before it can become the lock file.
        draft.sync_all()?;
        Ok(())
    }

    /// Offer the draft as the lock file: it succeeds only while none is there.
    fn link_into(&self, path: &Path) -> io::Result<()> {
        fs::hard_link(&self.path, path)
    }
}

impl Drop for Draft {
    /// Remove the draft name. A successful claim already gave the inode a second
    /// name, so this deletes nothing the lock depends on, and a draft left by a
    /// crash is a name nobody can use for anything.
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// The two facts `/proc` keeps about a process: its state letter, and the tick
/// count at which it began.
fn proc_stat(pid: u32) -> Option<(char, u64)> {
    began_and_started_at(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Read the state letter and the start tick off one `/proc/<pid>/stat` line.
///
/// The command name is field 2, it is wrapped in parentheses, and it may itself
/// hold a space or a parenthesis, so the fields after it are counted from the
/// last `)` rather than from the front of the line. `starttime` is field 22 of
/// the whole line, which is field `START_TIME_FIELD` of what is left after that
/// parenthesis.
fn began_and_started_at(stat: &str) -> Option<(char, u64)> {
    let close = stat.rfind(')')?;
    let after_comm = stat.get(close + 1..)?;
    let mut fields = after_comm.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let mut fields = after_comm.split_whitespace();
    let started = fields.nth(START_TIME_FIELD)?.parse().ok()?;
    Some((state, started))
}

/// The tick count at which `pid` began, if the machine can say.
fn process_start_ticks(pid: u32) -> Option<u64> {
    proc_stat(pid).map(|(_, started)| started)
}

/// The identifier of this boot, if the machine can say.
fn boot_id() -> Option<String> {
    let written = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let id = written.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// An identifier no other process is expected to produce, for one acquire.
///
/// `RandomState` is seeded from the machine every time it is built, so hashing
/// two fresh seedings with this process's identity and clock gives a value that
/// differs per process and per call. That is all it is asked for: tell two lock
/// records apart, so a release can prove which file it owns. Nothing here needs
/// an answer an adversary could not be kept away from by the file mode.
fn token() -> String {
    let seeds = RandomState::new();
    let other = RandomState::new().build_hasher().finish();
    let mut hasher = seeds.build_hasher();
    hasher.write_u32(std::process::id());
    let elapsed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    hasher.write_u32(elapsed.subsec_nanos());
    hasher.write_u64(other);
    format!("{:016x}", hasher.finish())
}

/// The instant now, in the one spelling a lock record stores.
///
/// ADR-0012 decided that an instant with no text to store is a serialization
/// failure rather than a panic, and this follows it: only a clock set outside
/// the range `time` represents reaches the refusal.
fn written_now() -> Result<String> {
    OffsetDateTime::from(SystemTime::now())
        .to_utc()
        .format(&Rfc3339)
        .map_err(|unformattable| {
            Error::Serde(serde_json::Error::custom(format_args!(
                "the clock reading now has no RFC 3339 spelling to store in a lock record: \
                 {unformattable}"
            )))
        })
}

/// How long to sleep before asking the filesystem again: at most the poll
/// interval, and never past the caller's deadline. A caller with no deadline has
/// nothing to stop short of, so it waits a whole interval each time.
fn up_to_deadline(deadline: Option<Instant>) -> Duration {
    let Some(until) = deadline else {
        return POLL_INTERVAL;
    };
    let remaining = until.saturating_duration_since(Instant::now());
    if remaining < POLL_INTERVAL {
        remaining
    } else {
        POLL_INTERVAL
    }
}

/// The refusal once a wait has used everything it was given.
fn gave_up(path: &Path, why: &Why, waited: Duration) -> Error {
    Error::Io(io::Error::new(
        ErrorKind::TimedOut,
        format!(
            "timed out after {} waiting for the repository lock at `{}`: {why}",
            waited_for(waited),
            path.display()
        ),
    ))
}

/// A wait, phrased for whoever has to decide whether to keep waiting.
fn waited_for(waited: Duration) -> String {
    if waited.as_secs() >= 1 {
        format!("{:.3} s", waited.as_secs_f64())
    } else {
        format!("{} ms", waited.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Abandoned, Error, LOCK_FILE_MODE, Record, acquire, began_and_started_at, boot_id,
        lock_path, process_start_ticks, waited_for,
    };
    use std::fmt::Display;
    use std::fs;
    use std::io::ErrorKind;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    /// The program a fixture holder runs: nothing, slowly, under its own pid.
    const SLEEP: &str = "/bin/sleep";

    /// A span that is surely a later kernel tick than the instant it is
    /// measured from. The kernel counts a process's start in hundredths of a
    /// second, so two processes begun inside the same hundredth share a start
    /// tick; a fixture that has to look younger than this process asks to be.
    const A_TICK_LATER: Duration = Duration::from_millis(25);

    /// The instant every planted record claims to have been written at.
    const PLANTED_SINCE: &str = "2015-06-30T07:08:09Z";

    /// The token every planted record carries, so a test can tell a file it
    /// planted from a file an `acquire` wrote.
    const PLANTED_TOKEN: &str = "planted-token";

    /// A directory to lock: `register` makes a run's state directory, a test
    /// makes one with the same standing.
    fn lockable() -> TempDir {
        tempfile::tempdir().expect("a temporary directory is creatable")
    }

    /// The name `lock_path` appends, so a test never repeats the constant.
    fn lock_name() -> String {
        lock_path(Path::new("/"))
            .file_name()
            .expect("the lock path ends in a file name")
            .to_string_lossy()
            .into_owned()
    }

    /// When this process began, which only the kernel knows.
    fn this_start_tick() -> u64 {
        process_start_ticks(std::process::id()).expect("the machine says when this process began")
    }

    /// A record as if `pid` had written it.
    fn planted(pid: u32, started: Option<u64>, boot: Option<&str>) -> Record {
        Record {
            pid,
            started_ticks: started,
            boot_id: boot.map(str::to_owned),
            token: PLANTED_TOKEN.to_owned(),
            since: PLANTED_SINCE.to_owned(),
        }
    }

    /// Write `record` as the lock file of `dir`, and hand back its path.
    fn plant(dir: &Path, record: &Record) -> PathBuf {
        let path = lock_path(dir);
        let written = serde_json::to_vec(record).expect("a record is serializable");
        fs::write(&path, written)
            .unwrap_or_else(|refused| panic!("`{}` should be writable: {refused}", path.display()));
        path
    }

    /// Write `record` with one field this build has never heard of, the way a
    /// newer ktask-rs would, and hand back the lock file's path.
    fn plant_a_record_with_a_field_nobody_here_knows(dir: &Path, record: &Record) -> PathBuf {
        let path = lock_path(dir);
        let mut written: serde_json::Value =
            serde_json::to_value(record).expect("a record is serializable");
        written
            .as_object_mut()
            .expect("a serialized record is a json object")
            .insert("intent".to_owned(), serde_json::Value::from("publish"));
        fs::write(&path, written.to_string())
            .unwrap_or_else(|refused| panic!("`{}` should be writable: {refused}", path.display()));
        path
    }

    /// A `/proc/<pid>/stat` line as the kernel writes one: `state` in field 3 and
    /// `started` in field 22, with the eighteen fields the kernel puts between
    /// them spelled out, so that a test can name the two facts this module reads
    /// without reading the machine's own process table.
    /// `started` is `Display` rather than a number so a test can put something
    /// that is not one in the field.
    fn stat_line(comm: &str, state: char, started: impl Display) -> String {
        format!(
            "4321 ({comm}) {state} 1 4321 4321 0 -1 4194560 100 0 0 0 10 20 0 0 20 0 1 0 \
             {started} 0 0 0"
        )
    }

    /// A real second process, alive, and its record as its own file would hold it.
    struct Stray {
        child: Child,
        record: Record,
    }

    impl Stray {
        fn spawn() -> Self {
            let child = Command::new(SLEEP)
                .arg("20")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("the fixture holder starts");
            let pid = child.id();
            let record = Record {
                pid,
                started_ticks: process_start_ticks(pid),
                boot_id: boot_id(),
                token: PLANTED_TOKEN.to_owned(),
                since: PLANTED_SINCE.to_owned(),
            };
            Self { child, record }
        }

        /// The pid, for a test that stops it from another thread.
        fn nix_pid(&self) -> nix::unistd::Pid {
            nix::unistd::Pid::from_raw(
                i32::try_from(i64::from(self.record.pid))
                    .expect("the child has a pid the kernel accepts"),
            )
        }

        /// Stop the process and reap it, so its pid is gone from the table, and
        /// hand back the record it left behind.
        fn stop(&mut self) -> Record {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.record.clone()
        }
    }

    impl Drop for Stray {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// The names below `dir`, sorted, so a test can say what an acquire left.
    fn entries(dir: &Path) -> Vec<String> {
        let reading = fs::read_dir(dir)
            .unwrap_or_else(|refused| panic!("`{}` should be readable: {refused}", dir.display()));
        let mut names = reading
            .map(|entry| {
                entry
                    .expect("the directory entry is readable")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<String>>();
        names.sort();
        names
    }

    /// The whole text of a lock file, for a test that compares it before and after.
    fn text_at(path: &Path) -> String {
        fs::read_to_string(path)
            .unwrap_or_else(|refused| panic!("`{}` should be readable: {refused}", path.display()))
    }

    /// The token a lock file names, so a test can say whose lock it still is.
    fn token_at(path: &Path) -> String {
        let text = text_at(path);
        let record: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|unreadable| {
            panic!("`{}` should hold a record: {unreadable}", path.display())
        });
        record["token"]
            .as_str()
            .expect("a record names its holder's token")
            .to_owned()
    }

    /// The `io` reason of a refusal: how a caller tells contention from a
    /// broken filesystem.
    fn reason_of(error: &Error) -> ErrorKind {
        let Error::Io(cause) = error else {
            panic!("the refusal should be an io error, it is `{error}`");
        };
        cause.kind()
    }

    #[test]
    fn acquiring_writes_one_lock_file_that_names_its_holder_by_pid_and_start_time() {
        let dir = lockable();
        let before = time::OffsetDateTime::from(std::time::SystemTime::now());
        let lock = acquire(dir.path(), Duration::from_secs(1))
            .expect("an unlocked directory is locked without waiting");
        let after = time::OffsetDateTime::from(std::time::SystemTime::now());

        assert_eq!(lock.path(), lock_path(dir.path()));
        assert!(
            lock.reclaimed().is_none(),
            "nothing was locked before this, so there is nothing to report"
        );

        let written: serde_json::Value =
            serde_json::from_str(&text_at(lock.path())).expect("a lock file is one json object");
        assert_eq!(written["pid"], serde_json::Value::from(std::process::id()));
        assert_eq!(
            written["started_ticks"],
            serde_json::Value::from(this_start_tick()),
            "the record has to say when its holder began: that is what tells a \
             reused pid from the process that wrote the file"
        );
        assert_eq!(
            written["boot_id"],
            serde_json::Value::from(boot_id().expect("this boot is named"))
        );
        let since = time::OffsetDateTime::parse(
            written["since"].as_str().expect("an instant is written"),
            &time::format_description::well_known::Rfc3339,
        )
        .expect("the instant the record was written is an RFC 3339 instant");
        assert!(before <= since && since <= after, "the record is dated now");
        assert!(
            !written["token"]
                .as_str()
                .expect("a token is written")
                .is_empty(),
            "the record has to name this file uniquely, or a release cannot prove it is ours"
        );
        assert_eq!(entries(dir.path()), vec![lock_name()]);
    }

    #[test]
    fn the_lock_file_is_readable_only_by_the_user_running_the_supervisor() {
        let dir = lockable();
        let lock = acquire(dir.path(), Duration::ZERO).expect("an unlocked directory is locked");
        let mode = fs::metadata(lock.path())
            .expect("the lock file is there")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, LOCK_FILE_MODE);
    }

    #[test]
    fn a_second_acquire_waits_for_a_live_holder_and_gives_up_naming_it() {
        let dir = lockable();
        let stray = Stray::spawn();
        let path = plant(dir.path(), &stray.record);

        let started = Instant::now();
        let refused = acquire(dir.path(), Duration::from_millis(150))
            .expect_err("a live holder does not release inside 150 ms");
        let waited = started.elapsed();

        assert_eq!(
            reason_of(&refused),
            ErrorKind::TimedOut,
            "contention has to be distinguishable from a broken filesystem"
        );
        let message = refused.to_string();
        assert!(
            message.contains(&format!("pid {}", stray.record.pid)),
            "the refusal names the holder: {message}"
        );
        assert!(
            message.contains(PLANTED_SINCE),
            "the refusal says how long it has been held: {message}"
        );
        assert!(
            message.contains(&path.display().to_string()),
            "the refusal says which file: {message}"
        );
        assert!(
            waited >= Duration::from_millis(150),
            "the caller asked to wait, and gave up after {waited:?}"
        );
        assert_eq!(
            token_at(&path),
            PLANTED_TOKEN,
            "a lock a live process holds is not touched"
        );
    }

    #[test]
    fn a_zero_timeout_reports_the_holder_without_waiting_at_all() {
        let dir = lockable();
        let stray = Stray::spawn();
        plant(dir.path(), &stray.record);

        let started = Instant::now();
        let refused = acquire(dir.path(), Duration::ZERO)
            .expect_err("a held lock is refused with no budget to wait");

        assert_eq!(reason_of(&refused), ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "a zero timeout waits: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn an_acquire_takes_the_lock_as_soon_as_the_live_holder_stops() {
        let dir = lockable();
        let stray = Stray::spawn();
        let planted_path = plant(dir.path(), &stray.record);
        let stopped = stray.nix_pid();
        let killer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            let _ = nix::sys::signal::kill(stopped, nix::sys::signal::Signal::SIGKILL);
        });

        let started = Instant::now();
        let lock = acquire(dir.path(), Duration::from_secs(5))
            .expect("the lock is free once its holder stops");
        let waited = started.elapsed();
        killer
            .join()
            .expect("the thread that stopped the holder finishes");

        let reclaimed = lock
            .reclaimed()
            .expect("taking a lock from a process that stopped is reported");
        assert_eq!(
            reclaimed.reason,
            Abandoned::DeadPid,
            "a stopped holder is stopped whether or not its parent has reaped it"
        );
        assert!(
            waited < Duration::from_secs(4),
            "the wait ended when the holder stopped, not when the timeout did: {waited:?}"
        );
        assert_ne!(token_at(&planted_path), PLANTED_TOKEN);
    }

    #[test]
    fn a_timeout_no_instant_can_carry_waits_for_the_holder_instead_of_overflowing() {
        let dir = lockable();
        let stray = Stray::spawn();
        plant(dir.path(), &stray.record);
        let stopped = stray.nix_pid();
        let killer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            let _ = nix::sys::signal::kill(stopped, nix::sys::signal::Signal::SIGKILL);
        });

        // A timeout is a `u64` number of seconds in the configuration, and a
        // `Duration` built from the largest of them is a caller asking to wait as
        // long as the holder takes. Adding it to an `Instant` overflows, so this
        // has to be a wait with no deadline — not a panic, and not a refusal.
        let lock = acquire(dir.path(), Duration::MAX)
            .expect("a timeout too large to name a deadline waits rather than overflowing");
        killer
            .join()
            .expect("the thread that stopped the holder finishes");

        assert!(
            lock.reclaimed().is_some(),
            "the wait ran on until the holder stopped and took the lock over"
        );
    }

    #[test]
    fn a_lock_whose_pid_has_stopped_is_reclaimed_and_the_reclamation_is_reported() {
        let dir = lockable();
        let mut stray = Stray::spawn();
        let record = stray.stop();
        let path = plant(dir.path(), &record);

        let started = Instant::now();
        let lock = acquire(dir.path(), Duration::from_millis(500))
            .expect("a lock left by a process that has stopped is taken over");
        let waited = started.elapsed();

        let reclaimed = lock
            .reclaimed()
            .expect("the takeover is reported, not passed over in silence");
        assert_eq!(reclaimed.path, path);
        assert_eq!(reclaimed.pid, record.pid);
        assert_eq!(reclaimed.since, PLANTED_SINCE);
        assert_eq!(reclaimed.reason, Abandoned::DeadPid);
        let report = reclaimed.to_string();
        assert!(
            report.contains(&format!("pid {}", record.pid)) && report.contains("not running"),
            "the report says who left it and why it is theirs to take: {report}"
        );
        assert!(
            waited < Duration::from_millis(400),
            "an abandoned lock is taken at once, not after the wait: {waited:?}"
        );
        assert_ne!(
            token_at(&path),
            PLANTED_TOKEN,
            "the file names its new holder"
        );
    }

    #[test]
    fn a_reused_pid_is_reclaimed_because_the_record_names_an_other_process() {
        let dir = lockable();
        let now = this_start_tick();
        let recorded = now.saturating_sub(4_000);
        let record = planted(std::process::id(), Some(recorded), boot_id().as_deref());
        let path = plant(dir.path(), &record);

        let lock = acquire(dir.path(), Duration::from_millis(500))
            .expect("a pid that outlived its holder does not keep the lock");
        let reclaimed = lock
            .reclaimed()
            .expect("a lock taken from a reused pid is reported");
        assert_eq!(
            reclaimed.reason,
            Abandoned::PidReused { recorded, now },
            "a live pid is not proof of a live holder: only the start time says \
             whether the process behind it is the one that wrote the file"
        );
        assert!(
            reclaimed.to_string().contains("ticks after boot"),
            "the report says how it knows: {reclaimed}"
        );
        assert_ne!(token_at(&path), PLANTED_TOKEN);
    }

    #[test]
    fn a_lock_written_before_this_boot_is_reclaimed_although_its_pid_is_running() {
        let dir = lockable();
        let other_boot = "00000000-1111-2222-3333-444444444444";
        let record = planted(
            std::process::id(),
            Some(this_start_tick()),
            Some(other_boot),
        );
        let path = plant(dir.path(), &record);

        let lock = acquire(dir.path(), Duration::from_millis(500))
            .expect("a lock that predates this boot cannot name a live holder");
        let reclaimed = lock.reclaimed().expect("the takeover is reported");
        assert_eq!(
            reclaimed.reason,
            Abandoned::Rebooted {
                recorded: other_boot.to_owned(),
                now: boot_id().expect("this boot is named"),
            }
        );
        assert!(
            reclaimed.to_string().contains("boot"),
            "the report says what it found: {reclaimed}"
        );
        assert_ne!(token_at(&path), PLANTED_TOKEN);
    }

    #[test]
    fn a_lock_written_by_another_users_process_is_waited_behind_and_never_removed() {
        let dir = lockable();
        let init = 1;
        let record = planted(init, process_start_ticks(init), boot_id().as_deref());
        let path = plant(dir.path(), &record);

        let refused = acquire(dir.path(), Duration::from_millis(150))
            .expect_err("a process this one cannot examine keeps its lock");
        assert_eq!(reason_of(&refused), ErrorKind::TimedOut);
        assert!(
            refused.to_string().contains(&format!("pid {init}")),
            "the refusal names the holder: {refused}"
        );
        assert_eq!(
            token_at(&path),
            PLANTED_TOKEN,
            "a lock this process cannot prove abandoned is left alone"
        );
    }

    #[test]
    fn a_lock_whose_record_cannot_be_read_is_waited_behind_and_left_in_place() {
        let dir = lockable();
        let path = lock_path(dir.path());
        fs::write(&path, b"pid = somebody else's business")
            .expect("the fixture lock file is writable");

        let refused = acquire(dir.path(), Duration::from_millis(150))
            .expect_err("an unreadable lock is not an available lock");

        assert_eq!(reason_of(&refused), ErrorKind::TimedOut);
        assert!(
            refused.to_string().contains("could not be read"),
            "the refusal says what it could not do: {refused}"
        );
        assert_eq!(
            fs::read(&path).expect("the lock file is readable"),
            b"pid = somebody else's business",
            "a holder that cannot be identified is never deleted"
        );
    }

    #[test]
    fn a_record_with_a_field_this_build_does_not_know_is_still_a_holder_to_wait_behind() {
        let dir = lockable();
        let stray = Stray::spawn();
        let path = plant_a_record_with_a_field_nobody_here_knows(dir.path(), &stray.record);

        let refused = acquire(dir.path(), Duration::from_millis(150))
            .expect_err("a record from a newer build names a live holder");

        assert_eq!(reason_of(&refused), ErrorKind::TimedOut);
        assert!(
            refused.to_string().contains("it is held by pid"),
            "an unknown field is not an unreadable record: {refused}"
        );
        assert_eq!(
            token_at(&path),
            PLANTED_TOKEN,
            "a record this build only partly understands is still somebody's lock"
        );
    }

    #[test]
    fn a_record_that_cannot_say_when_its_holder_began_is_waited_behind() {
        let dir = lockable();
        let path = lock_path(dir.path());
        fs::write(
            &path,
            format!(
                "{{\"pid\":{},\"token\":\"{PLANTED_TOKEN}\",\"since\":\"{PLANTED_SINCE}\"}}",
                std::process::id()
            ),
        )
        .expect("the fixture lock file is writable");

        let refused = acquire(dir.path(), Duration::from_millis(150))
            .expect_err("a holder that may still be running is a holder");

        assert_eq!(reason_of(&refused), ErrorKind::TimedOut);
        assert!(
            refused.to_string().contains("it is held by pid"),
            "an unknown start time means keep waiting, not that the holder is gone: {refused}"
        );
        assert_eq!(
            token_at(&path),
            PLANTED_TOKEN,
            "the machine's silence is not evidence of an abandoned lock"
        );
    }

    #[test]
    fn a_record_naming_pid_zero_is_waited_behind_rather_than_signalled() {
        let dir = lockable();
        let path = plant(dir.path(), &planted(0, Some(1), boot_id().as_deref()));

        let refused = acquire(dir.path(), Duration::from_millis(150))
            .expect_err("pid 0 is every process in this group, not a holder");

        assert_eq!(reason_of(&refused), ErrorKind::TimedOut);
        assert!(
            refused.to_string().contains("could not be read"),
            "a record naming no process is unreadable, not held: {refused}"
        );
        assert_eq!(token_at(&path), PLANTED_TOKEN);
    }

    #[test]
    fn a_pid_too_large_to_signal_is_waited_behind_and_never_removed() {
        let dir = lockable();
        let path = plant(
            dir.path(),
            &planted(u32::MAX, Some(1), boot_id().as_deref()),
        );

        let refused = acquire(dir.path(), Duration::from_millis(150))
            .expect_err("a pid this platform cannot name is not a pid it may probe");

        assert_eq!(reason_of(&refused), ErrorKind::TimedOut);
        assert!(
            refused.to_string().contains("pid"),
            "the refusal still says what is in the way: {refused}"
        );
        assert_eq!(token_at(&path), PLANTED_TOKEN);
    }

    #[test]
    fn one_process_is_not_handed_the_lock_twice() {
        let dir = lockable();
        let held = acquire(dir.path(), Duration::ZERO).expect("an unlocked directory is locked");
        let written = text_at(held.path());

        let refused = acquire(dir.path(), Duration::from_millis(150))
            .expect_err("holding the lock does not entitle a second holder");

        assert_eq!(reason_of(&refused), ErrorKind::TimedOut);
        assert!(
            refused
                .to_string()
                .contains(&format!("pid {}", std::process::id())),
            "the refusal names the holder, which here is this same process: {refused}"
        );
        assert_eq!(text_at(held.path()), written, "the holder keeps its lock");
        drop(held);
    }

    #[test]
    fn dropping_the_lock_hands_it_to_the_next_acquire_without_waiting() {
        let dir = lockable();
        let first = acquire(dir.path(), Duration::ZERO).expect("an unlocked directory is locked");
        let written = text_at(first.path());
        let path = lock_path(dir.path());

        drop(first);
        assert!(!path.exists(), "dropping the lock releases it");

        let started = Instant::now();
        let second = acquire(dir.path(), Duration::ZERO)
            .expect("the next holder takes a released lock without waiting");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "the lock was free: {:?}",
            started.elapsed()
        );
        assert_ne!(
            text_at(second.path()),
            written,
            "the new holder wrote its own record"
        );
    }

    #[test]
    fn releasing_the_lock_removes_the_file_at_once() {
        let dir = lockable();
        let lock = acquire(dir.path(), Duration::ZERO).expect("an unlocked directory is locked");
        let path = lock.path().to_path_buf();

        lock.release().expect("giving a lock back is not a failure");
        assert!(
            !path.exists(),
            "the lock is given back, not merely forgotten"
        );
        acquire(dir.path(), Duration::ZERO).expect("a released lock is free for the next holder");
    }

    #[test]
    fn a_lock_another_process_wrote_in_our_place_survives_our_drop() {
        let dir = lockable();
        let ours = acquire(dir.path(), Duration::ZERO).expect("an unlocked directory is locked");
        plant(dir.path(), &planted(7, None, boot_id().as_deref()));

        drop(ours);
        assert_eq!(
            token_at(&lock_path(dir.path())),
            PLANTED_TOKEN,
            "a file that no longer names us is not ours to delete"
        );
    }

    #[test]
    fn releasing_a_lock_another_process_wrote_in_our_place_is_refused() {
        let dir = lockable();
        let ours = acquire(dir.path(), Duration::ZERO).expect("an unlocked directory is locked");
        plant(dir.path(), &planted(7, None, boot_id().as_deref()));

        let refused = ours
            .release()
            .expect_err("releasing a lock that is no longer ours has to be said, not done");
        assert!(matches!(refused, Error::Policy { .. }), "{refused}");
        assert!(
            refused.to_string().contains(&lock_name()),
            "the refusal names the file it would have deleted: {refused}"
        );
        assert_eq!(token_at(&lock_path(dir.path())), PLANTED_TOKEN);
    }

    #[test]
    fn releasing_a_lock_that_was_removed_behind_our_back_is_reported() {
        let dir = lockable();
        let ours = acquire(dir.path(), Duration::ZERO).expect("an unlocked directory is locked");
        fs::remove_file(ours.path()).expect("the fixture removes the lock file");

        let refused = ours
            .release()
            .expect_err("a lock that vanished while held is a fact about the run, not a success");
        assert!(matches!(refused, Error::Policy { .. }), "{refused}");
        assert!(
            refused.to_string().contains(&lock_name()),
            "the refusal names the file that is not there: {refused}"
        );
    }

    #[test]
    fn an_acquire_leaves_nothing_but_the_lock_file_whether_it_took_the_lock_or_gave_up() {
        let dir = lockable();
        let held = acquire(dir.path(), Duration::ZERO).expect("an unlocked directory is locked");
        assert_eq!(entries(dir.path()), vec![lock_name()]);

        let refused = acquire(dir.path(), Duration::from_millis(120))
            .expect_err("a held lock is refused by the second caller too");
        assert_eq!(reason_of(&refused), ErrorKind::TimedOut);
        assert_eq!(
            entries(dir.path()),
            vec![lock_name()],
            "the loser's draft record is not left in the directory"
        );
        drop(held);
        assert!(
            entries(dir.path()).is_empty(),
            "the directory is left as it was found: {:?}",
            entries(dir.path())
        );
    }

    #[test]
    fn acquiring_refuses_a_directory_that_is_not_there_and_names_it() {
        let dir = lockable();
        let absent = dir.path().join("absent");

        let refused = acquire(&absent, Duration::from_secs(1))
            .expect_err("nothing is locked in a directory that does not exist");

        assert!(matches!(refused, Error::NotFound { .. }), "{refused}");
        assert!(
            refused.to_string().contains("absent"),
            "the refusal names what is missing: {refused}"
        );
        assert!(
            entries(dir.path()).is_empty(),
            "a refused acquire creates nothing"
        );
    }

    #[test]
    fn two_directories_are_locked_independently_of_each_other() {
        let one = lockable();
        let two = lockable();

        let first = acquire(one.path(), Duration::ZERO).expect("the first directory locks");
        let second = acquire(two.path(), Duration::from_millis(200))
            .expect("a busy directory does not block another one");
        assert_eq!(first.path(), lock_path(one.path()));
        assert_eq!(second.path(), lock_path(two.path()));

        drop(first);
        assert!(!lock_path(one.path()).exists());
        assert_eq!(
            token_at(&lock_path(two.path())),
            token_at(second.path()),
            "releasing one directory leaves the other held"
        );
    }

    #[test]
    fn simultaneous_acquirers_hold_the_lock_one_at_a_time() {
        let dir = lockable();
        let dir_path = dir.path();
        let inside = Arc::new(AtomicBool::new(false));
        let clashes = Arc::new(Mutex::new(Vec::<String>::new()));
        let holders = 4;
        let rounds = 5;

        thread::scope(|scope| {
            for _ in 0..holders {
                let inside = Arc::clone(&inside);
                let clashes = Arc::clone(&clashes);
                scope.spawn(move || {
                    for _ in 0..rounds {
                        let lock = acquire(dir_path, Duration::from_secs(20))
                            .expect("every holder gets the lock in turn");
                        if inside.swap(true, Ordering::SeqCst) {
                            clashes
                                .lock()
                                .expect("the ledger of clashes is sound")
                                .push(format!("two holders inside `{}`", lock.path().display()));
                        }
                        thread::sleep(Duration::from_millis(2));
                        inside.store(false, Ordering::SeqCst);
                    }
                });
            }
        });

        let ledger = clashes.lock().expect("the ledger of clashes is sound");
        assert!(ledger.is_empty(), "{}", ledger.join("; "));
        assert!(
            entries(dir_path).is_empty(),
            "every holder released what it took"
        );
    }

    #[test]
    fn the_state_and_start_tick_are_read_from_after_a_command_name_holding_a_parenthesis() {
        // `starttime` is the 22nd field of the line, and field 2 is the command
        // name in parentheses — which may itself hold a space or a parenthesis.
        // Counted from the front of the line instead of from the last `)`, the
        // same reader returns some other field and calls it the holder's start
        // time, which is how a live pid gets mistaken for a reused one.
        assert_eq!(
            began_and_started_at(&stat_line("ktask-rs gate", 'S', 987_654)),
            Some(('S', 987_654))
        );
        assert_eq!(
            began_and_started_at(&stat_line("(odd) (name)", 'Z', 4_242)),
            Some(('Z', 4_242)),
            "a command name that closes a parenthesis early must not move the fields"
        );
        assert_eq!(
            began_and_started_at("7 (sh) Z 1 7"),
            None,
            "a line that stops short of the start tick says nothing"
        );
        assert_eq!(
            began_and_started_at(&stat_line("sh", 'S', "not-a-tick")),
            None,
            "a field that is not a number is not a start tick either"
        );
    }

    #[test]
    fn the_start_time_read_for_a_pid_orders_processes_by_when_they_began() {
        let ours = this_start_tick();
        let init = process_start_ticks(1).expect("pid 1 reports when it began");
        // Without the pause the holder is begun in the same hundredth of a
        // second as this process, and the assertion below would be a coin toss
        // rather than a statement about start times.
        thread::sleep(A_TICK_LATER);
        let stray = Stray::spawn();
        let theirs = stray
            .record
            .started_ticks
            .expect("a process that was just started is already in the table");

        assert!(
            init < ours,
            "pid 1 began before this process: {init} vs {ours}"
        );
        assert!(
            ours < theirs,
            "the holder the test just started began after this process: {ours} vs {theirs}"
        );
    }

    #[test]
    fn the_wait_is_reported_in_milliseconds_until_it_passes_a_second() {
        assert_eq!(waited_for(Duration::ZERO), "0 ms");
        assert_eq!(waited_for(Duration::from_millis(145)), "145 ms");
        assert_eq!(waited_for(Duration::from_millis(1_500)), "1.500 s");
        assert_eq!(waited_for(Duration::from_secs(2)), "2.000 s");
    }
}
