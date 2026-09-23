//! The evidence one attempt left behind, kept as the attempt produced it.
//!
//! VISION.md §6 makes an attempt's record a thing in its own right: "every
//! attempt is preserved separately: executor session ID, timestamps,
//! configured and provider-reported model IDs, exit reason, commands run, gate
//! results, git SHAs, tokens, and cost." [`AttemptRecord`] is that sentence
//! turned into a type: one row per attempt, holding the evidence and nothing
//! that could only be guessed at.
//!
//! # Why the journal holds it
//!
//! `docs/DESIGN.md` gives the database four tables, and `events` is the only
//! append-only one: `task_state` is a projection that a rebuild drops and
//! recomputes from the events alone (ADR-0024). So an attempt's evidence is
//! durable only if it is an event. Written to a table of its own it would be
//! evidence a rebuild could not reproduce, and a repair that loses an attempt's
//! evidence is the failure this design exists to prevent. The catalog entry is
//! [`crate::EventKind::AttemptRecorded`], whose single payload field is the record.
//!
//! Two clocks appear and neither replaces the other. The envelope's `ts` is
//! when the record was journaled, stamped by the journal as it appended;
//! [`AttemptRecord::started`] and [`AttemptRecord::ended`] are when the attempt
//! itself ran, which is what a cost of an attempt or a session that outlived
//! its supervisor is measured from.
//!
//! # Why a retry adds a record
//!
//! Nothing keys a record by task alone: [`AttemptRecord::id`] is the attempt's
//! own number within the task, and every record carries it. A second attempt
//! therefore produces a second record rather than a newer value in the first
//! one's place, which is what VISION.md §7 means by preserving the prior
//! attempt's evidence across a remediation — a failure bundle cannot be
//! assembled from a record that the retry overwrote, and a failures screen
//! cannot show what the first attempt tried. The journal layer enforces the
//! same shape from the other side: its two triggers refuse the update and the
//! delete that would collapse two attempts into one row (ADR-0017).
//!
//! # What the optional halves mean
//!
//! Every `Option` here is a fact that may genuinely not have happened, not a
//! placeholder, and `None` is never written as a zero — the rule ADR-0049
//! established for [`Usage`] and [`GateResult`] keeps. So
//! [`AttemptRecord::candidate_sha`] is `None` for an attempt that produced no
//! commit, [`AttemptRecord::ended`] is `None` for one that never reported a
//! stop, and [`AttemptRecord::usage`] distinguishes an attempt that never had a
//! session to ask from one whose session reported nothing: the second is
//! `Some(Usage::unavailable())`, whose every figure is `None`, and the first is
//! `None`, which is not the same claim about the run.
//!
//! [`AttemptRecord::model_configured`] and [`AttemptRecord::model_reported`]
//! are two fields rather than one because they are two witnesses: the
//! configuration's wording and the session's own answer, which ADR-0057 decided
//! are recorded side by side and a mismatch refused as a configuration error.
//! Merging them would lose the only evidence that the provider ran something
//! other than what it was told to.
//!
//! # Reading them back
//!
//! [`crate::attempt_records`] answers a task's whole attempt history: every
//! record it ever journaled, in the order the attempts ran, read out of the rows
//! that carry them. Ordered by attempt rather than by sequence because a record
//! is filed when an attempt's recorder reaches it, and read for exactly one task
//! at a time — a record naming a task other than the row it was filed under is
//! refused with that row's sequence number rather than quietly dropped, since a
//! reader cannot be handed evidence and told which half of it to disbelieve.
//!
//! # Where the evidence goes on disk
//!
//! The journal is the source of truth; the directory is the receipt.
//! [`crate::write_evidence`] gives an attempt a home of its own at
//! `<state_dir>/attempts/<task>/<attempt>/`, holding `report.md`, `context.md`,
//! `gates/<kind>.log` and `record.json`, and [`crate::read_evidence`] reads that
//! directory back. Two readers of one attempt's evidence, and they agree because
//! both are written from one [`AttemptRecord`]: the journal holds what was
//! recorded (ADR-0063), the directory holds what the run left behind.
//!
//! The order of the writes is what makes an interruption visible. `record.json`
//! goes last, so its presence is the statement that the report, the context and
//! the gate logs are there too; a directory without it is a write that stopped
//! halfway, which [`crate::read_evidence`] refuses as damage and
//! [`crate::write_evidence`] rewrites whole rather than merging into. The same
//! rule the journal keeps is kept here: nothing is accepted on an
//! agent's say-so, and a half-truth is refused rather than read as a fact.
//!
//! The directory also outlives a repair of the state. A rebuild drops the
//! materialized projection and recomputes it from the events alone (ADR-0024), and
//! an evidence directory is not that projection — a task's reports and gate logs
//! are where they were when the projection was rebuilt, which is what the failure
//! screens and the human reading them need.
//!
//! Two ids name two levels of the layout rather than two suffixes of one filename,
//! so a retry adds a directory beside the first attempt's instead of replacing it
//! (VISION.md §7 assembles a remediation's bundle from the attempt the retry
//! replaced). An attempt that already has a record refuses a second, different one
//! at the file layer as well as at the journal's. Every file is redacted as it is
//! written and every directory is created owner-only, so evidence can be opened by
//! the account that ran the attempt and by nobody else; the redaction uses the
//! built-in pattern table, the gap ADR-0065 records rather than hides.
//!
//! # What is not here
//!
//! VISION.md §6 also lists "commands run". The record shape this task was given
//! does not carry it, and no field of [`GateResult`] holds one either: a gate
//! result names the [`crate::GateKind`] that ran and reports what the command wrote,
//! while the words themselves live in [`crate::Gate::command`], which is configuration
//! read before the run rather than evidence the run produced. The gap is
//! reported rather than papered over here — adding a field to a record is a
//! change to durable data, and the catalog entry that carries it is fixed in
//! `docs/DESIGN.md`. A journal that kept a command's words would also be keeping
//! the secret in a command line: `docs/DESIGN.md` redacts journal text, and
//! VISION.md §16 counts an operational command line among what must not leak.
//! The diff summary VISION.md §7 wants in a failure bundle is absent for the
//! same reason: a diff is the git layer's to produce, not a record's to invent.

use serde::ser::Error as _;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions, Permissions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::redact::{redact, redact_json};
use crate::{AttemptId, Error, EventKind, GateResult, Journal, Project, Result, TaskId, Usage};

/// Every attempt `task` made, each as its own record, in attempt order.
///
/// A task's history is a list rather than a row: [`Journal`] is append-only, and
/// its two triggers refuse the update and the delete that would collapse a retry
/// into the first attempt's place (ADR-0017). So reading a task's attempts is
/// reading everything it ever journaled, not reading its latest answer — which
/// is what VISION.md §7 needs to assemble a failure bundle from an attempt the
/// next one replaced.
///
/// The answer is ordered by [`AttemptRecord::id`] rather than by the order the
/// rows were written. Evidence is filed when an attempt's recorder gets to it,
/// so a journal can hold a later attempt before an earlier one, while the order
/// a reader comparing one attempt with the next wants is the order the attempts
/// ran. The sort is stable, so records no ordering explains stay as they were
/// written.
///
/// # Errors
///
/// [`Error::Corrupt`] when a row filed under `task` carries a record naming a
/// different task. Which of the two is wrong is not this function's to decide —
/// the row's task is what the recorder claimed and [`AttemptRecord::task`] is
/// what the attempt said — and showing a record as evidence about a task that
/// never ran it is exactly the thing this read exists to refuse, so the row is
/// refused with its sequence number rather than skipped. The journal's own read
/// errors pass through unchanged.
pub fn attempt_records(journal: &Journal, task: TaskId) -> Result<Vec<AttemptRecord>> {
    let mut records = Vec::new();
    for event in journal.events_for(task)? {
        if let EventKind::AttemptRecorded { record } = &event.kind {
            if record.task != task {
                return Err(Error::Corrupt {
                    detail: format!(
                        "attempt {} is journaled under task {task} but names task {}",
                        record.id, record.task
                    ),
                    seq: Some(event.seq.get()),
                });
            }
            records.push((**record).clone());
        }
    }
    records.sort_by_key(|held| held.id);
    Ok(records)
}

/// Everything one run of one task proved, as one durable row.
///
/// This is durable data in the sense [`crate::TaskState`] is: the journal stores it in
/// `serde`'s default representation and `--json` output prints it, so the field
/// names are what a reader six months from now has to go on. It is written once
/// per attempt and never updated — see the module header for why a retry is a
/// second record rather than a rewrite of the first.
///
/// The order of the fields is the order the evidence arrives in: the attempt is
/// identified, then timed, then what it was told and what it answered, then how
/// it stopped, then what the gates and the provider said, then the two commits
/// it sits between.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptRecord {
    /// Which run of [`AttemptRecord::task`] this is, 1-based within the task.
    /// A retry gets a new one, and with it a record of its own.
    pub id: AttemptId,
    /// The task this attempt ran. Held inside the record as well as in the
    /// journal row that carries it, so evidence that names no task cannot be
    /// filed against a task that never ran it.
    pub task: TaskId,
    /// When the attempt started, in UTC. The instant the agent's process was
    /// started, which the journal's own stamp is not: that one records when the
    /// record was written, and the two differ whenever evidence is filed after
    /// the fact.
    pub started: OffsetDateTime,
    /// When the attempt stopped, or `None` when it never reported a stop — an
    /// interrupted run whose process was found dead has no end instant of its
    /// own, and a record written while an attempt still runs reports none.
    pub ended: Option<OffsetDateTime>,
    /// The model id the configuration asked for, or `None` when the provider was
    /// started with none and ran on its own default.
    pub model_configured: Option<String>,
    /// The model id the session reported for itself, or `None` when the adapter
    /// finished holding nothing. Kept beside [`AttemptRecord::model_configured`]
    /// rather than in place of it (ADR-0057).
    pub model_reported: Option<String>,
    /// The provider's own id for the session, so a run can be traced into the
    /// executor's logs, or `None` when no session was ever opened.
    pub session_id: Option<String>,
    /// How the attempt ended, in the words of whoever observed it. Deliberately
    /// text: [`crate::FailureClass`] is the classified half of a failure and is
    /// journaled with the transition that failed the task, while this is the
    /// sentence beside it — which is also why an attempt that finished green
    /// has one ("exited 0", "session closed") rather than nothing.
    pub exit_reason: String,
    /// Every gate that ran against this attempt, in the order it ran, passing
    /// gates included. Empty for an attempt that stopped before the gates
    /// opened, which is why the list is kept rather than dropped: an empty list
    /// is itself the evidence that no gate was ever run.
    pub gates: Vec<GateResult>,
    /// What the session reported it spent, or `None` when there was no session
    /// to report it. A reported-nothing session is
    /// [`Usage::unavailable`], not `None`.
    pub usage: Option<Usage>,
    /// The commit this attempt started from — the one `PreflightPassed` recorded
    /// and every later commit is checked against. Not optional: an attempt with
    /// no base cannot be attributed to a tree, which is what preflight exists to
    /// establish before an agent is let near one.
    pub base_sha: String,
    /// The commit this attempt produced, or `None` when it produced none or
    /// none was proved to be on the remote. Held separately from
    /// [`AttemptRecord::base_sha`] because "the work is committed" is a fact
    /// with its own evidence: VISION.md §10 does not consider a publication
    /// real until the remote has been read back holding it.
    pub candidate_sha: Option<String>,
}

/// The directory every task's evidence sits in, below a project's state directory.
///
/// `pub(crate)` rather than private because [`crate::assemble`] names the same
/// layout in the report path it hands an agent: two spellings of one layout is
/// how a prompt and an evidence directory end up disagreeing (ADR-0075).
pub(crate) const EVIDENCE_ROOT: &str = "attempts";

/// The directory of one attempt's gate output, inside its evidence directory.
const GATES_DIR: &str = "gates";

/// The artifact naming what an attempt knew. It is written last, so its presence
/// is what says the rest of the directory is there.
const RECORD_FILE: &str = "record.json";

/// The artifact an operator or a screen reads first.
const REPORT_FILE: &str = "report.md";

/// The artifact holding the context document the attempt was given.
const CONTEXT_FILE: &str = "context.md";

/// The suffix of one gate's log, named for the [`crate::GateKind`] that ran.
const LOG_SUFFIX: &str = ".log";

/// The mode bits of every directory the evidence layout makes: owner-only, the
/// rule `project.rs` keeps a state directory to (VISION.md §11).
const EVIDENCE_DIR_MODE: u32 = 0o700;

/// The mode bits of every evidence file. Agent output is the least shareable
/// thing a run produces.
const EVIDENCE_FILE_MODE: u32 = 0o600;

/// Where one attempt's evidence lives.
///
/// `<state_dir>/attempts/<task>/<attempt>/`, each of the last two levels named by
/// the bare number [`TaskId`] and [`AttemptId`] print as. The task comes before
/// the attempt because evidence is read per task — a failures screen lists a
/// task's attempts and a repair opens one of them — and the attempt is a level
/// rather than a suffix of a filename so that a retry adds a directory instead of
/// replacing a file (VISION.md §7).
///
/// The layout is public because it is the deliverable: a later screen, a retention
/// sweep or an operator's shell all need to name the same directory without
/// spelling it out a second time.
#[must_use]
pub fn evidence_dir(project: &Project, task: TaskId, attempt: AttemptId) -> PathBuf {
    project
        .state_dir
        .join(EVIDENCE_ROOT)
        .join(task.to_string())
        .join(attempt.to_string())
}

/// Make one attempt's evidence directory exist, and own its own mode, without
/// filing anything in it.
///
/// [`write_evidence`] creates the same levels on its way to writing a record, and
/// this is the same guarantee asked for on its own: the run has to be able to say
/// *the directory named in the prompt is there* before a provider is started, so
/// that a session told to write `<dir>/agent-report.md` meets a directory rather
/// than an error. A report that could not be written is indistinguishable from an
/// agent that wrote none, which is why the promise is made before the session
/// rather than discovered after it.
///
/// Only the attempt's own two levels are made, and nothing is removed or
/// re-written: unlike [`write_evidence`] there is no torn record to notice here,
/// and an attempt's directory may already hold a record, gate logs and a report
/// this call has no business disturbing.
///
/// # Errors
///
/// [`Error::NotFound`] when the project has no state directory — registration
/// makes that directory and sets its mode, and an evidence writer does not invent
/// one. [`Error::Policy`] when a level of the layout is there and is not a
/// directory, or is reached through a symbolic link. [`Error::Io`] when the
/// filesystem refused to make or permission a level.
pub(crate) fn ensure_evidence_dir(
    project: &Project,
    task: TaskId,
    attempt: AttemptId,
) -> Result<()> {
    ensure_state_directory(project)?;
    for level in levels_below(&evidence_dir(project, task, attempt), &project.state_dir) {
        private_dir(&level)?;
    }
    Ok(())
}

/// File one attempt's evidence below its project's state directory.
///
/// The directory is [`evidence_dir`]'s layout, holding `report.md` (the record in
/// the words an operator reads), `context.md` (the context the attempt was given),
/// `gates/<kind>.log` (one log per gate kind, every run of it that this attempt
/// made) and `record.json` ([`AttemptRecord`] as JSON). The record is written
/// last, so a directory holding it holds the rest too.
///
/// Everything is redacted on the way in — the record, the context, the report and
/// each gate log — and every directory is created owner-only. Redaction uses the
/// built-in pattern table: this is the shape the task fixes, and a configured
/// `secret_patterns` list would have to be read from the project's configuration
/// to be honoured here. That gap is recorded in ADR-0065 rather than hidden.
///
/// # Re-filing an attempt
///
/// Writing the same record again is a repair: the same bytes go back, and a mode
/// somebody opened up since is set again. Writing a *different* record for an
/// attempt that already has one is refused with [`Error::Policy`] before anything
/// is created, because a remediation reads a retry's evidence out of the attempt
/// it replaced: an attempt that can answer twice is one whose earlier answer can
/// be lost. A directory whose `record.json` cannot be read back is a write that
/// was interrupted — that file is written last — and it is written whole again
/// rather than merged into, so no artifact of an older shape survives beside the
/// new one.
///
/// # Errors
///
/// [`Error::NotFound`] when the project's state directory is not there: evidence
/// has no home before a project is registered. [`Error::Policy`] when the attempt
/// already has a record that says something else, or when a level of the layout is
/// there and is not a directory. [`Error::Io`] when the filesystem refused the
/// write, and [`Error::Serde`] for the two cases in which a record or a clock
/// reading has no JSON or RFC 3339 spelling.
pub fn write_evidence(project: &Project, record: &AttemptRecord, context: &str) -> Result<()> {
    ensure_state_directory(project)?;
    let written = record_text(record)?;
    let dir = evidence_dir(project, record.task, record.id);
    let torn = match stored_record(&dir, &written)? {
        Stored::Absent | Stored::Complete => false,
        Stored::Contradicted => {
            return Err(Error::Policy {
                detail: format!(
                    "attempt {} of task {} already has evidence at `{}`; one attempt writes \
                     one record, and the one already there says something different",
                    record.id,
                    record.task,
                    dir.display()
                ),
                paths: vec![dir],
            });
        }
        Stored::Torn => true,
    };

    let gates = prepare_directory(&dir, &project.state_dir, torn)?;
    write_private(&dir.join(CONTEXT_FILE), &redact(context, &[]))?;
    for (kind, log) in gate_logs(&record.gates) {
        write_private(&gates.join(format!("{kind}{LOG_SUFFIX}")), &log)?;
    }
    write_private(&dir.join(REPORT_FILE), &report(record)?)?;
    write_private(&dir.join(RECORD_FILE), &written)
}

/// Every attempt `task` has filed evidence for, oldest attempt first.
///
/// The directory is read, not the journal: [`crate::attempt_records`] answers the
/// same question from the rows the journal holds, and the two agree because both
/// are written from one record (ADR-0063). The directory is what a run left
/// behind, which is why it survives a rebuild of the materialized state — a
/// rebuild recomputes the projection and touches no evidence (ADR-0024).
///
/// A task with no evidence directory reads back as no attempts: a task that never
/// ran, or one whose run died before any evidence was filed, is not a failure to
/// read. The journal stays the source of truth for whether a task ran at all.
///
/// # Errors
///
/// [`Error::Corrupt`] when the layout does not hold what its name claims — an
/// entry that is not an attempt directory, a directory with no `record.json`, a
/// record that cannot be read back, or a record naming an attempt or a task other
/// than the directory filed it under. Each refusal names the path a repair starts
/// from. [`Error::Io`] comes through when the filesystem refused the read for a
/// reason other than absence.
pub fn read_evidence(project: &Project, task: TaskId) -> Result<Vec<AttemptRecord>> {
    let root = project.state_dir.join(EVIDENCE_ROOT).join(task.to_string());
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(why) if refused_because_absent(&why) => return Ok(Vec::new()),
        Err(why) => return Err(why.into()),
    };

    let mut filed = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(number) = name.parse::<u32>().ok() else {
            return Err(not_an_attempt_directory(&path, task));
        };
        if !entry.file_type()?.is_dir() {
            return Err(not_an_attempt_directory(&path, task));
        }
        filed.push(read_record(&path, task, number)?);
    }
    filed.sort_by_key(|filed| filed.id);
    Ok(filed)
}

/// What an attempt's directory already says about it.
enum Stored {
    /// Nothing is filed yet, which is the ordinary case of one write.
    Absent,
    /// The record the caller is about to write: a re-filing, not a change.
    Complete,
    /// A record that reads back and says something different.
    Contradicted,
    /// Half a record, which is an interrupted write.
    Torn,
}

/// Ask one attempt's directory what it holds, without writing anything.
fn stored_record(dir: &Path, written: &str) -> Result<Stored> {
    let bytes = match fs::read(dir.join(RECORD_FILE)) {
        Ok(bytes) => bytes,
        Err(why) if refused_because_absent(&why) => return Ok(Stored::Absent),
        Err(why) => return Err(why.into()),
    };
    if bytes.as_slice() == written.as_bytes() {
        return Ok(Stored::Complete);
    }
    match serde_json::from_slice::<AttemptRecord>(&bytes) {
        Ok(_) => Ok(Stored::Contradicted),
        Err(_) => Ok(Stored::Torn),
    }
}

/// Make one attempt's evidence directory ready to be written into: every level of
/// the layout this module owns exists and is owner-only, and anything an
/// interrupted write left behind is gone.
///
/// The `gates` directory is made even for an attempt that ran no gate, because an
/// empty one is the evidence that none ran. The answer is that directory, which
/// the caller writes each [`GateResult`] into.
fn prepare_directory(dir: &Path, state_dir: &Path, torn: bool) -> Result<PathBuf> {
    for level in levels_below(dir, state_dir) {
        private_dir(&level)?;
    }
    if torn {
        fs::remove_dir_all(dir)?;
        private_dir(dir)?;
    }
    let gates = dir.join(GATES_DIR);
    private_dir(&gates)?;
    Ok(gates)
}

/// `path` and every ancestor of it below `root`, outermost first.
///
/// These are the levels [`write_evidence`] creates, and no level above them: a
/// state directory belongs to a registration, which made it and set its mode, and
/// an evidence writer has no business re-creating or re-permissioning it.
fn levels_below(path: &Path, root: &Path) -> Vec<PathBuf> {
    let mut levels = Vec::new();
    let mut cursor = Some(path);
    while let Some(current) = cursor {
        if current == root {
            break;
        }
        levels.push(current.to_path_buf());
        cursor = current.parent();
    }
    levels.reverse();
    levels
}

/// Make `path` exist as a directory kept at [`EVIDENCE_DIR_MODE`].
///
/// The mode is set rather than only asked for at creation: a directory that
/// already exists keeps whatever mode somebody gave it, and only the set takes a
/// grant back. A link is refused as an occupied path, since a link is not a
/// directory this module owns and writing through one would put evidence of a run
/// wherever the link points.
fn private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(occupied(path)),
        Err(why) if refused_because_absent(&why) => fs::create_dir(path)?,
        Err(why) => return Err(why.into()),
    }
    fs::set_permissions(path, Permissions::from_mode(EVIDENCE_DIR_MODE))?;
    Ok(())
}

/// Refuse a level of the layout that is there and is not a directory.
fn occupied(path: &Path) -> Error {
    Error::Policy {
        detail: format!(
            "`{}` is already there and is not a directory",
            path.display()
        ),
        paths: vec![path.to_path_buf()],
    }
}

/// Require a registered project's state directory to be the directory a
/// registration made it.
fn ensure_state_directory(project: &Project) -> Result<()> {
    match fs::symlink_metadata(&project.state_dir) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(occupied(&project.state_dir)),
        Err(why) if refused_because_absent(&why) => Err(Error::NotFound {
            what: format!(
                "state directory of project {} ({})",
                project.id,
                project.state_dir.display()
            ),
        }),
        Err(why) => Err(why.into()),
    }
}

/// Whether the filesystem says the path simply is not there.
///
/// `NotADirectory` is the same answer for a writer: a level above the path is not
/// a directory, so nothing this module filed can be below it.
fn refused_because_absent(why: &io::Error) -> bool {
    matches!(
        why.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Write `text` to `path`, kept at [`EVIDENCE_FILE_MODE`], and flush it to the
/// storage before answering.
///
/// The data is synced rather than left in the page cache because this is the
/// evidence a later human judges a run by: an attempt whose report was lost to a
/// power cut is a run that cannot be reviewed, which is the failure VISION.md §11
/// restrictive permissions and durable state exist to prevent.
///
/// `pub(crate)` rather than private because [`crate::file_report`] files a
/// remediation's account in the same layout and has to keep it to the same mode
/// and the same durability: two spellings of a durable private write is how one
/// of them ends up un-synced (the reason `EVIDENCE_ROOT` is widened too).
pub(crate) fn write_private(path: &Path, text: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(EVIDENCE_FILE_MODE)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    fs::set_permissions(path, Permissions::from_mode(EVIDENCE_FILE_MODE))?;
    file.sync_all()?;
    Ok(())
}

/// The record as `record.json` holds it: one JSON line, redacted, ended.
fn record_text(record: &AttemptRecord) -> Result<String> {
    let encoded = serde_json::to_string(record)?;
    Ok(format!("{}\n", redact_json(&encoded, &[])?))
}

/// Read the record one attempt's directory holds, and check it is that attempt's.
///
/// The directory name and the record's own two ids have to agree: a record filed
/// under a task it does not name would be handed to a reader as evidence about a
/// task that never ran it, which is the refusal [`crate::attempt_records`] makes of
/// a journal row. The directory is not the journal and gets the same answer.
fn read_record(path: &Path, task: TaskId, number: u32) -> Result<AttemptRecord> {
    let file = path.join(RECORD_FILE);
    let bytes = match fs::read(&file) {
        Ok(bytes) => bytes,
        Err(why) if refused_because_absent(&why) => {
            return Err(Error::Corrupt {
                detail: format!(
                    "`{}` is attempt {number} of task {task} and holds no `{RECORD_FILE}`, \
                     which its writer writes last, so the write was interrupted",
                    path.display()
                ),
                seq: None,
            });
        }
        Err(why) => return Err(why.into()),
    };
    let filed: AttemptRecord =
        serde_json::from_slice(&bytes).map_err(|unparsable| Error::Corrupt {
            detail: format!(
                "`{}` holds attempt {number} of task {task} as a `{RECORD_FILE}` that cannot be \
             read back: {unparsable}",
                path.display()
            ),
            seq: None,
        })?;
    if filed.id != AttemptId::new(number) || filed.task != task {
        return Err(Error::Corrupt {
            detail: format!(
                "`{}` holds attempt {} of task {}, not attempt {number} of task {task}",
                path.display(),
                filed.id,
                filed.task
            ),
            seq: None,
        });
    }
    Ok(filed)
}

/// Refuse an entry in a task's evidence directory that is not an attempt's
/// directory.
fn not_an_attempt_directory(path: &Path, task: TaskId) -> Error {
    Error::Corrupt {
        detail: format!(
            "`{}` sits in the evidence directory of task {task}, which holds one directory \
             per attempt named by its number",
            path.display()
        ),
        seq: None,
    }
}

/// One attempt's report: its record in the words an operator reads.
fn report(record: &AttemptRecord) -> Result<String> {
    let started = instant_text(record.started)?;
    let ended = match record.ended {
        Some(stopped) => instant_text(stopped)?,
        None => "no stop reported".to_owned(),
    };
    let composed = format!("{}\n", fact_lines(record, &started, &ended).join("\n"));
    Ok(redact(&composed, &[]))
}

/// The report's lines, in the order the evidence arrives in.
fn fact_lines(record: &AttemptRecord, started: &str, ended: &str) -> Vec<String> {
    vec![
        format!("# task {} attempt {}", record.task, record.id),
        String::new(),
        format!("- started: {started}"),
        format!("- ended: {ended}"),
        format!(
            "- model configured: {}",
            or_none(record.model_configured.as_deref())
        ),
        format!(
            "- model reported: {}",
            or_none(record.model_reported.as_deref())
        ),
        format!("- session: {}", or_none(record.session_id.as_deref())),
        format!("- exit reason: {}", one_line(&record.exit_reason)),
        format!("- base commit: {}", record.base_sha),
        format!(
            "- candidate commit: {}",
            or_none(record.candidate_sha.as_deref())
        ),
        format!("- usage: {}", spent(record.usage.as_ref())),
        format!("- gates: {}", gate_summary(&record.gates)),
    ]
}

/// An instant in the one spelling the journal writes its timestamps in.
fn instant_text(instant: OffsetDateTime) -> Result<String> {
    instant.format(&Rfc3339).map_err(|unformattable| {
        Error::Serde(serde_json::Error::custom(format_args!(
            "an attempt's instant has no RFC 3339 spelling to store: {unformattable}"
        )))
    })
}

/// A fact a record may not hold, in the words a report reads as.
fn or_none(value: Option<&str>) -> String {
    value.map_or_else(|| "none".to_owned(), str::to_owned)
}

/// Free text as the one line a report field is: every run of whitespace folded.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// What a session reported spending, or why nobody knows.
///
/// A figure that was never reported is left out rather than written as a zero,
/// and the two empties are told apart (ADR-0049).
fn spent(usage: Option<&Usage>) -> String {
    let Some(usage) = usage else {
        return "unasked".to_owned();
    };
    let mut figures = Vec::new();
    if let Some(input) = usage.input_tokens {
        figures.push(format!("in={input}"));
    }
    if let Some(output) = usage.output_tokens {
        figures.push(format!("out={output}"));
    }
    if let Some(cached) = usage.cached_tokens {
        figures.push(format!("cached={cached}"));
    }
    if let Some(cost) = usage.cost_usd {
        figures.push(format!("cost=${cost}"));
    }
    if figures.is_empty() {
        return "unreported".to_owned();
    }
    figures.join(" ")
}

/// Every gate that ran, on the report's one `gates:` line.
fn gate_summary(gates: &[GateResult]) -> String {
    if gates.is_empty() {
        return "none".to_owned();
    }
    gates
        .iter()
        .map(gate_fact)
        .collect::<Vec<String>>()
        .join("; ")
}

/// One gate run as the report states it.
fn gate_fact(gate: &GateResult) -> String {
    format!(
        "{} {} ({}, {}ms)",
        gate.kind.as_str(),
        verdict_word(gate),
        gate_verdict(gate),
        gate.duration_ms
    )
}

/// Whether a gate passed, in the one word a report uses for it.
fn verdict_word(gate: &GateResult) -> &'static str {
    if gate.passed { "passed" } else { "refused" }
}

/// How a gate stopped, in the one form that says why it is a failure.
///
/// A timeout is its own fact and the kill that enforced it is not the failure; a
/// process stopped by a signal has no exit code to name, and a gate that reported
/// neither has neither.
fn gate_verdict(gate: &GateResult) -> String {
    if gate.timed_out {
        return "timed out".to_owned();
    }
    if let Some(signal) = gate.signal {
        return format!("signal {signal}");
    }
    if let Some(code) = gate.exit_code {
        return format!("exit {code}");
    }
    String::from("stopped without a verdict")
}

/// One log per gate kind, in the order the kinds first ran.
///
/// A protocol can run one gate twice in one attempt — a `tdd` protocol's targeted
/// gate goes red and then green — and VISION.md §9 makes both runs evidence of the
/// same attempt, so both sections go in the one file named for the kind.
fn gate_logs(gates: &[GateResult]) -> Vec<(String, String)> {
    let mut logs: Vec<(String, Vec<String>)> = Vec::new();
    for gate in gates {
        let kind = gate.kind.as_str().to_owned();
        let section = gate_section(gate);
        match logs.iter_mut().find(|(filed, _)| *filed == kind) {
            Some((_, sections)) => sections.push(section),
            None => logs.push((kind, vec![section])),
        }
    }
    logs.into_iter()
        .map(|(kind, sections)| (kind, sections.join("\n")))
        .collect()
}

/// One gate run as its log holds it: the verdict line, then what it wrote.
///
/// The output is kept as it was written rather than trimmed, because the
/// whitespace of a `cargo` failure is part of what a human compares a rerun
/// against. Standard error is labelled when there is any, since nothing else in
/// the file says which half of a command's output a line came from.
fn gate_section(gate: &GateResult) -> String {
    let verdict = gate_fact(gate);
    let mut section = format!("gate {verdict}\n{}", gate.stdout);
    if !gate.stderr.is_empty() {
        section.push_str("-- stderr --\n");
        section.push_str(&gate.stderr);
    }
    let mut written = redact(&section, &[]);
    if !written.ends_with('\n') {
        written.push('\n');
    }
    written
}

#[cfg(test)]
mod tests {
    use super::{AttemptRecord, evidence_dir, read_evidence, write_evidence};
    use crate::redact::{MASK, fixtures::github_token};
    use crate::{
        AttemptId, Error, EventKind, GateKind, GateResult, Journal, PauseReason, Phase, Project,
        TaskId, Usage, UsageSource, attempt_records, journal_path,
    };
    use serde_json::Value;
    use std::fs;
    use std::io::ErrorKind;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;
    use time::macros::datetime;

    /// The twelve field names a record is written with, spelled out a second
    /// time from the task that fixed the shape, so a rename has to be decided
    /// twice to become durable data.
    const FIELDS: [&str; 12] = [
        "id",
        "task",
        "started",
        "ended",
        "model_configured",
        "model_reported",
        "session_id",
        "exit_reason",
        "gates",
        "usage",
        "base_sha",
        "candidate_sha",
    ];

    /// The commit an attempt started from, and the one it produced: two
    /// different texts, because a record that wrote the same sha into both would
    /// pass a test that only checked one of them came back.
    const BASE: &str = "0b78d3f1c2a4";
    const CANDIDATE: &str = "b7d1f3a9e5c2";

    /// One gate that refused and one that passed, so a record holding two keeps
    /// both their orders and their halves rather than the loudest one.
    fn gates() -> Vec<GateResult> {
        vec![
            GateResult {
                kind: GateKind::Verify,
                passed: false,
                exit_code: Some(101),
                signal: None,
                duration_ms: 41_930,
                stdout: "test attempt::tests::records .. FAILED\n".to_owned(),
                stderr: String::new(),
                timed_out: false,
            },
            GateResult {
                kind: GateKind::Format,
                passed: true,
                exit_code: Some(0),
                signal: None,
                duration_ms: 940,
                stdout: String::new(),
                stderr: "Diff in src/attempt.rs at line 12:\n".to_owned(),
                timed_out: false,
            },
        ]
    }

    /// What the provider's own telemetry said one attempt spent.
    fn reported_usage() -> Usage {
        Usage {
            input_tokens: Some(12_000),
            output_tokens: Some(3_400),
            cached_tokens: Some(9_000),
            cost_usd: Some(0.42),
            source: UsageSource::Provider,
        }
    }

    /// A second attempt of task 7: told one model, answering with another id,
    /// with every optional half filled in.
    fn record() -> AttemptRecord {
        AttemptRecord {
            id: AttemptId::new(2),
            task: TaskId::new(7),
            started: datetime!(2026-09-20 09:14:03.5 UTC),
            ended: Some(datetime!(2026-09-20 09:41:47 UTC)),
            model_configured: Some("gpt-5.6-sol".to_owned()),
            model_reported: Some("gpt-5.6-sol-2026-09-01".to_owned()),
            session_id: Some("sess_01HQZK".to_owned()),
            exit_reason: "gate verify failed: 2 tests refused".to_owned(),
            gates: gates(),
            usage: Some(reported_usage()),
            base_sha: BASE.to_owned(),
            candidate_sha: Some(CANDIDATE.to_owned()),
        }
    }

    /// The record's own encoding as the object the journal would store.
    fn encoded(record: &AttemptRecord) -> Value {
        serde_json::to_value(record).expect("an attempt record encodes as JSON")
    }

    /// Encodes `record` and reads the text straight back.
    fn through_json(record: &AttemptRecord) -> AttemptRecord {
        let text = serde_json::to_string(record).expect("an attempt record encodes as text");
        serde_json::from_str(&text).expect("what a record writes is read back")
    }

    #[test]
    fn an_attempt_record_keeps_every_field_the_journal_stored() {
        let written = record();
        let object = encoded(&written)
            .as_object()
            .expect("a record is a JSON object")
            .clone();
        let mut carried: Vec<&str> = object.keys().map(String::as_str).collect();
        carried.sort_unstable();
        let mut documented = FIELDS.to_vec();
        documented.sort_unstable();
        assert_eq!(
            carried, documented,
            "a record must carry exactly the fields the shape fixes, or a journal holds \
             evidence a reader has no name for"
        );

        assert_eq!(
            through_json(&written),
            written,
            "every field of a record must survive the form the journal stores it in"
        );

        // The two commits are kept apart, the two model ids are kept apart, and
        // the two gate results keep the order they ran in: each pair is one a
        // summary would happily collapse into one.
        let read = through_json(&written);
        assert_eq!(read.base_sha, BASE, "the commit the attempt started from");
        assert_eq!(
            read.candidate_sha.as_deref(),
            Some(CANDIDATE),
            "the commit the attempt produced is not the base one"
        );
        assert_eq!(
            (read.model_configured, read.model_reported),
            (
                Some("gpt-5.6-sol".to_owned()),
                Some("gpt-5.6-sol-2026-09-01".to_owned())
            ),
            "a reported model id is evidence about the session, not a copy of the \
             configured one (ADR-0057)"
        );
        assert_eq!(
            read.gates.iter().map(|gate| gate.kind).collect::<Vec<_>>(),
            vec![GateKind::Verify, GateKind::Format],
            "gates are kept in the order they ran, passing ones included"
        );
        assert_eq!(read.usage, Some(reported_usage()), "what the session spent");
        assert_eq!(read.session_id.as_deref(), Some("sess_01HQZK"));
        assert_eq!(
            read.exit_reason, "gate verify failed: 2 tests refused",
            "the sentence beside the classification is kept word for word"
        );

        // An instant is written the way this vocabulary already writes one,
        // rather than in a form of this record's own.
        let vocabulary = serde_json::to_value(PauseReason::Limit {
            until: Some(datetime!(2026-09-20 09:14:03.5 UTC)),
        })
        .expect("a pause reason encodes as JSON");
        assert_eq!(
            object.get("started"),
            vocabulary.get("Limit").and_then(|half| half.get("until")),
            "an attempt's own instant must be spelled the way a state spells one"
        );
        assert_eq!(
            through_json(&written).started,
            written.started,
            "and it survives to the nanosecond, because that is what two attempts \
             started in the same second are told apart by"
        );
    }

    #[test]
    fn an_attempt_record_writes_a_field_it_did_not_observe_as_null_not_as_a_zero() {
        let bare = AttemptRecord {
            id: AttemptId::new(1),
            task: TaskId::new(7),
            started: datetime!(2026-09-20 09:14:03 UTC),
            ended: None,
            model_configured: None,
            model_reported: None,
            session_id: None,
            exit_reason: "exited 0".to_owned(),
            gates: Vec::new(),
            usage: None,
            base_sha: BASE.to_owned(),
            candidate_sha: None,
        };
        let object = encoded(&bare)
            .as_object()
            .expect("a record is a JSON object")
            .clone();

        for absent in [
            "ended",
            "model_configured",
            "model_reported",
            "session_id",
            "usage",
            "candidate_sha",
        ] {
            assert_eq!(
                object.get(absent),
                Some(&Value::Null),
                "{absent} was not observed, so it is written as null and stays a key: a \
                 row this tool writes always carries both halves"
            );
        }
        assert_eq!(
            object.get("gates"),
            Some(&Value::Array(Vec::new())),
            "an attempt that never reached a gate records that, rather than omitting \
             the list and leaving it to be guessed"
        );

        let read = through_json(&bare);
        assert_eq!(
            read, bare,
            "and every half comes back as the nothing it was"
        );
        assert_eq!(
            read.usage, None,
            "an attempt with no session to ask is not the same run as one whose session \
             reported nothing, which would be Some(Usage::unavailable())"
        );
        assert_eq!(
            read.candidate_sha, None,
            "no commit is never written as the empty sha, which reads as a sha"
        );
    }

    #[test]
    fn an_attempt_record_refuses_a_key_the_type_does_not_have() {
        let mut with_extra = encoded(&record());
        with_extra
            .as_object_mut()
            .expect("a record is a JSON object")
            .insert("commands".to_owned(), Value::Array(Vec::new()));
        assert!(
            serde_json::from_value::<AttemptRecord>(with_extra).is_err(),
            "a field the shape does not fix is refused rather than decoded without it, \
             the way a configuration document refuses one"
        );

        for required in ["base_sha", "gates", "exit_reason", "started", "task"] {
            let mut missing = encoded(&record());
            missing
                .as_object_mut()
                .expect("a record is a JSON object")
                .remove(required);
            assert!(
                serde_json::from_value::<AttemptRecord>(missing).is_err(),
                "{required} is required: a record missing it is refused rather than \
                 invented from a placeholder"
            );
        }
    }

    /// The same task's first attempt: an earlier instant, a session that never
    /// reported a model back, no commit, and no telemetry to report.
    fn first_attempt() -> AttemptRecord {
        AttemptRecord {
            id: AttemptId::new(1),
            task: TaskId::new(7),
            started: datetime!(2026-09-19 18:02:11 UTC),
            ended: Some(datetime!(2026-09-19 18:30:40 UTC)),
            model_configured: Some("gpt-5.6-sol".to_owned()),
            model_reported: None,
            session_id: Some("sess_01HQZJ".to_owned()),
            exit_reason: "exited 0".to_owned(),
            gates: Vec::new(),
            usage: Some(Usage::unavailable()),
            base_sha: BASE.to_owned(),
            candidate_sha: None,
        }
    }

    /// A journal in a scratch directory, and the directory that keeps it alive.
    fn journal() -> (tempfile::TempDir, Journal) {
        let parent = tempdir().expect("a scratch directory beside the repository");
        let journal = Journal::open(&journal_path(parent.path()))
            .expect("a new journal opens below the scratch directory");
        (parent, journal)
    }

    /// Files `record` under the task it names, which is what a recorder does: the
    /// row's task and the record's own task are the same task.
    fn file(journal: &mut Journal, record: &AttemptRecord) {
        journal
            .append(
                Some(record.task),
                &EventKind::AttemptRecorded {
                    record: Box::new(record.clone()),
                },
            )
            .expect("an attempt record appends, transition or not");
    }

    #[test]
    fn a_retry_adds_an_attempt_record_rather_than_overwriting_the_first() {
        let (parent, mut journal) = journal();
        let first = first_attempt();
        file(&mut journal, &first);
        file(&mut journal, &record());

        let read = attempt_records(&journal, TaskId::new(7)).expect("two records read back");
        assert_eq!(read.len(), 2, "one record per attempt, not the newest one");
        assert_eq!(
            read[0], first,
            "the first attempt's record is the record that was filed, not the retry's \
             answer filed into its place"
        );
        assert_eq!(
            (read[0].session_id.as_deref(), read[1].session_id.as_deref()),
            (Some("sess_01HQZJ"), Some("sess_01HQZK")),
            "two sessions of one task stay two sessions"
        );
        assert_eq!(
            (read[0].usage, read[1].usage),
            (Some(Usage::unavailable()), Some(reported_usage())),
            "what nobody reported and what the provider reported stay the two different \
             claims they are"
        );
        assert_eq!(
            (read[0].gates.len(), read[1].gates.len()),
            (0, 2),
            "an attempt that never reached a gate keeps an empty list beside an attempt \
             that reached two"
        );
        drop(parent);
    }

    #[test]
    fn attempt_records_read_back_in_attempt_order_whatever_order_they_were_journaled_in() {
        let (parent, mut journal) = journal();
        let mut third = record();
        third.id = AttemptId::new(3);
        third.candidate_sha = Some("c9e2a4d7f0b1".to_owned());

        // Filed late rather than first: a recorder files an attempt's evidence
        // when it gets to it, so a later attempt's row can be on disk before an
        // earlier attempt's is.
        file(&mut journal, &third);
        file(&mut journal, &first_attempt());
        file(&mut journal, &record());

        let read = attempt_records(&journal, TaskId::new(7)).expect("three records read back");
        assert_eq!(
            read.iter().map(|held| held.id.get()).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "attempt order, which is what a reader comparing one attempt with the next \
             one needs, and which journal order is not"
        );
        assert_eq!(
            read[2].candidate_sha.as_deref(),
            Some("c9e2a4d7f0b1"),
            "the record that was journaled first is still the last one read"
        );
        drop(parent);
    }

    #[test]
    fn attempt_records_name_the_task_they_were_read_for_and_no_other() {
        let (parent, mut journal) = journal();
        file(&mut journal, &first_attempt());
        let mut elsewhere = record();
        elsewhere.task = TaskId::new(8);
        file(&mut journal, &elsewhere);

        let seven = attempt_records(&journal, TaskId::new(7)).expect("one task's records");
        assert_eq!(
            seven.iter().map(|held| held.task).collect::<Vec<_>>(),
            vec![TaskId::new(7)],
            "reading one task reads that task's attempts and none of its neighbour's"
        );
        let eight = attempt_records(&journal, TaskId::new(8)).expect("the other task's records");
        assert_eq!(
            eight.iter().map(|held| held.id.get()).collect::<Vec<_>>(),
            vec![2],
            "the neighbour keeps the attempt it ran, unread from the task above it"
        );
        let none = attempt_records(&journal, TaskId::new(9))
            .expect("a task that never ran is an empty answer, not an error");
        assert!(
            none.is_empty(),
            "no records is the answer for a task with none, so a screen can render it \
             without a special case"
        );
        drop(parent);
    }

    #[test]
    fn an_attempt_record_journaled_under_a_task_it_does_not_name_is_refused() {
        let (parent, mut journal) = journal();
        let mut displaced = first_attempt();
        displaced.task = TaskId::new(8);
        journal
            .append(
                Some(TaskId::new(7)),
                &EventKind::AttemptRecorded {
                    record: Box::new(displaced),
                },
            )
            .expect("a record appends under the task its caller named");

        let error = attempt_records(&journal, TaskId::new(7))
            .expect_err("a record about another task cannot be read as evidence about this one");
        assert!(
            matches!(error, Error::Corrupt { seq: Some(1), .. }),
            "the refusal is the one that carries a location, because a repair starts by \
             opening the row it names"
        );
        assert_eq!(
            error.to_string(),
            "corrupt data at seq 1: attempt 1 is journaled under task 7 but names task 8",
            "the refusal names the row, the attempt and both tasks, because the repair \
            starts from which of the two is the one that is wrong"
        );
        drop(parent);
    }

    // ————— Where the evidence goes on disk —————

    /// Every path one attempt's evidence is made of, spelled out a second time
    /// from the task that fixed the layout: a name added or renamed has to be
    /// decided twice before it becomes the home every later screen reads.
    const LAYOUT: [&str; 8] = [
        "7/",
        "7/2/",
        "7/2/context.md",
        "7/2/gates/",
        "7/2/gates/format.log",
        "7/2/gates/verify.log",
        "7/2/record.json",
        "7/2/report.md",
    ];

    /// The context one attempt was given, holding the two shapes a stored
    /// context has to survive: a blank line inside it, and a trailing newline.
    const CONTEXT: &str = "# ktask context\n\nADR-0063: one record per attempt.\n";

    /// The mode every directory the evidence layout makes is kept at.
    const DIR_MODE: u32 = 0o700;

    /// The mode every evidence file is kept at.
    const FILE_MODE: u32 = 0o600;

    /// A project whose state directory is below `scratch` and already exists, as
    /// a registration leaves it.
    fn registered(scratch: &Path) -> Project {
        let id = "0123456789abcdef".to_owned();
        let state_dir = scratch.join("state").join(&id);
        fs::create_dir_all(&state_dir).expect("a state directory to write evidence below");
        Project {
            root: scratch.join("repository"),
            id,
            state_dir,
        }
    }

    /// The directory one attempt's evidence lives in, spelled out of the layout
    /// rather than through [`evidence_dir`], so a writer and a reader that
    /// disagree about the layout cannot agree with each other.
    fn attempt_dir(project: &Project, task: u32, attempt: u32) -> PathBuf {
        project
            .state_dir
            .join("attempts")
            .join(task.to_string())
            .join(attempt.to_string())
    }

    /// Every path below `root`, sorted, with a directory's own name ending in
    /// `/` so the shape of the tree is asserted and not only its files.
    fn tree(root: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).expect("the evidence tree is readable") {
                let entry = entry.expect("a readable directory entry");
                let child = entry.path();
                let name = child
                    .strip_prefix(root)
                    .expect("a path below the directory it was listed from")
                    .display()
                    .to_string();
                if entry
                    .file_type()
                    .expect("a readable directory entry")
                    .is_dir()
                {
                    found.push(format!("{name}/"));
                    stack.push(child);
                } else {
                    found.push(name);
                }
            }
        }
        found.sort();
        found
    }

    /// Every file of one attempt's evidence with its bytes, sorted by path: the
    /// whole directory, so an unchanged file cannot hide behind the one file a
    /// test happened to open.
    fn evidence(project: &Project, task: u32, attempt: u32) -> Vec<(String, Vec<u8>)> {
        let root = attempt_dir(project, task, attempt);
        let mut held: Vec<(String, Vec<u8>)> = tree(&root)
            .into_iter()
            .filter(|name| !name.ends_with('/'))
            .map(|name| {
                let bytes = fs::read(root.join(&name)).expect("an evidence file is readable");
                (name, bytes)
            })
            .collect();
        held.sort();
        held
    }

    /// The text of one evidence file.
    fn artifact(project: &Project, task: u32, attempt: u32, name: &str) -> String {
        let path = attempt_dir(project, task, attempt).join(name);
        fs::read_to_string(&path)
            .unwrap_or_else(|why| panic!("`{}` should be readable text: {why}", path.display()))
    }

    /// The mode bits of one path, with no file-type bits.
    fn mode(path: &Path) -> u32 {
        fs::metadata(path)
            .unwrap_or_else(|why| panic!("`{}` should be there: {why}", path.display()))
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn one_attempt_lays_its_evidence_below_the_state_directory() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());

        write_evidence(&project, &record(), CONTEXT).expect("an attempt writes its evidence");

        assert_eq!(
            tree(&project.state_dir.join("attempts")),
            LAYOUT,
            "the home of an attempt's evidence is `<state_dir>/attempts/<task>/<attempt>/` \
             holding `report.md`, `context.md`, `gates/<kind>.log` and `record.json`, and \
             it holds nothing else: a fifth artifact is an undocumented one"
        );
        assert_eq!(
            evidence_dir(&project, TaskId::new(7), AttemptId::new(2)),
            attempt_dir(&project, 7, 2),
            "the layout names a task's directory and then the attempt's, each by the bare \
             number its id prints as, and the helper that says so is the one the writer \
             used"
        );
    }

    #[test]
    fn the_evidence_tree_is_readable_only_by_the_user_running_the_supervisor() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());

        write_evidence(&project, &record(), CONTEXT).expect("an attempt writes its evidence");

        for name in ["", "7", "7/2", "7/2/gates"] {
            assert_eq!(
                mode(&project.state_dir.join("attempts").join(name)),
                DIR_MODE,
                "`attempts/{name}` holds an attempt's prompt, logs and report, so nothing \
                 is granted to the group or to other users"
            );
        }
        for name in [
            "report.md",
            "context.md",
            "record.json",
            "gates/verify.log",
            "gates/format.log",
        ] {
            assert_eq!(
                mode(&attempt_dir(&project, 7, 2).join(name)),
                FILE_MODE,
                "`{name}` is agent output, the least shareable thing a run produces"
            );
        }
    }

    #[test]
    fn writing_evidence_again_tightens_what_someone_left_open() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        let held = record();
        write_evidence(&project, &held, CONTEXT).expect("an attempt writes its evidence");

        let opened = project.state_dir.join("attempts");
        fs::set_permissions(&opened, fs::Permissions::from_mode(0o755))
            .expect("a directory can be opened up behind the supervisor's back");
        let report = attempt_dir(&project, 7, 2).join("report.md");
        fs::set_permissions(&report, fs::Permissions::from_mode(0o644))
            .expect("a file can be opened up behind the supervisor's back");

        write_evidence(&project, &held, CONTEXT)
            .expect("re-filing one attempt's evidence is a repair, not a refusal");

        assert_eq!(
            mode(&opened),
            DIR_MODE,
            "a mode asked for once at creation is a mode that can be lost after creation, \
             so every write sets it again"
        );
        assert_eq!(
            mode(&report),
            FILE_MODE,
            "the same is true of a file that already exists: creating it grants nothing, \
             and only the set takes a grant back"
        );
    }

    #[test]
    fn a_retry_adds_an_attempt_directory_rather_than_overwriting_the_first() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        let first = first_attempt();
        write_evidence(&project, &first, CONTEXT).expect("the first attempt writes evidence");
        let kept = evidence(&project, 7, 1);

        write_evidence(&project, &record(), CONTEXT).expect("the retry writes evidence too");

        assert_eq!(
            tree(&project.state_dir.join("attempts").join("7")),
            [
                "1/",
                "1/context.md",
                "1/gates/",
                "1/record.json",
                "1/report.md",
                "2/",
                "2/context.md",
                "2/gates/",
                "2/gates/format.log",
                "2/gates/verify.log",
                "2/record.json",
                "2/report.md",
            ],
            "a retry is a second directory beside the first and never a newer answer \
             written into the first one's place: VISION.md §7 assembles a failure bundle \
             from the attempt the retry replaced"
        );
        assert_eq!(
            evidence(&project, 7, 1),
            kept,
            "the first attempt's evidence is byte for byte what its own write left there"
        );

        let read = read_evidence(&project, TaskId::new(7)).expect("both attempts read back");
        assert_eq!(
            read,
            vec![first.clone(), record()],
            "one task's evidence reads back as one record per attempt, oldest attempt \
             first, each holding what it knew when it ran"
        );
        assert_eq!(
            (read[0].session_id.as_deref(), read[1].session_id.as_deref()),
            (Some("sess_01HQZJ"), Some("sess_01HQZK")),
            "two sessions of one task stay two sessions"
        );
    }

    #[test]
    fn evidence_survives_a_rebuild_of_the_materialized_state() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        let mut journal = Journal::open_for(&project).expect("a journal in the state directory");
        let held = record();
        let task = TaskId::new(7);

        journal
            .append(Some(task), &EventKind::PreflightStarted)
            .expect("preflight begins");
        journal
            .append(
                Some(task),
                &EventKind::PreflightPassed {
                    base_sha: BASE.to_owned(),
                },
            )
            .expect("preflight passes");
        journal
            .append(
                Some(task),
                &EventKind::AttemptStarted {
                    attempt: held.id,
                    protocol: "direct".to_owned(),
                    pid: 42_424,
                    base_sha: BASE.to_owned(),
                },
            )
            .expect("an attempt starts");
        journal
            .append(
                Some(task),
                &EventKind::PhaseEntered {
                    attempt: held.id,
                    phase: Phase::Implement,
                },
            )
            .expect("the attempt's phase opens, which is what puts the task in `running`");
        journal
            .append(
                Some(task),
                &EventKind::AttemptRecorded {
                    record: Box::new(held.clone()),
                },
            )
            .expect("its evidence is journaled while the attempt is held (ADR-0063)");
        write_evidence(&project, &held, CONTEXT).expect("the same evidence goes to disk");
        let kept = evidence(&project, 7, 2);

        journal
            .rebuild_state()
            .expect("the journal replays, so the projection can be rebuilt");

        assert_eq!(
            evidence(&project, 7, 2),
            kept,
            "a rebuild drops the materialized projection and recomputes it from the events \
             alone (ADR-0024); the evidence directory is not that projection, so an \
             attempt's report, context and gate logs outlive a repair of the state"
        );
        assert_eq!(
            read_evidence(&project, task).expect("the evidence still reads back"),
            vec![held.clone()],
            "the reader answers the same record after a rebuild as before one"
        );
        assert_eq!(
            attempt_records(&journal, task).expect("the journal answers too"),
            vec![held],
            "the two readers of one attempt's evidence agree about it: the journal holds \
             what was recorded, the directory holds what the run left behind"
        );
    }

    #[test]
    fn no_credential_an_attempt_spoke_reaches_the_evidence_files() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        let planted = github_token();
        let mut telling = record();
        telling.exit_reason = format!(
            "the push was refused after Authorization: Bearer {planted} for \
             https://github.com/IllyaYalovyy/ktask-rs.git"
        );
        let mut talking = gates();
        talking[0].stderr = format!("remote: support for the token {planted} expired");
        talking[1].stderr = format!("checked out the tree with {planted} in the url");
        telling.gates = talking;

        write_evidence(
            &project,
            &telling,
            &format!("context\nthe token is {planted}\n"),
        )
        .expect("an attempt whose agent talked still writes its evidence");

        for (name, bytes) in evidence(&project, 7, 2) {
            let written = String::from_utf8(bytes).expect("evidence is written as text");
            assert!(
                !written.contains(&planted),
                "`{name}` still holds the credential the attempt spoke"
            );
            assert!(
                written.contains(MASK),
                "`{name}` holds no mask either, so nothing was redacted rather than \
                 something being redacted: {written}"
            );
        }

        let read = read_evidence(&project, TaskId::new(7)).expect("the redacted record reads");
        assert_eq!(
            read[0].exit_reason,
            "the push was refused after Authorization: Bearer [redacted] for \
             https://github.com/IllyaYalovyy/ktask-rs.git",
            "the stored evidence says a credential was there and never says what it was, \
             and the address that says which remote refused is kept"
        );
        assert_eq!(
            read[0].gates[0].stderr, "remote: support for the token [redacted] expired",
            "a gate's output is redacted on its way into its log and into the record beside \
             it, so neither half carries the value"
        );
    }

    /// The second attempt's evidence, filed under attempt number `attempt` — the
    /// shape a third or fourth retry of one task leaves behind.
    fn numbered(attempt: u32) -> AttemptRecord {
        AttemptRecord {
            id: AttemptId::new(attempt),
            ..record()
        }
    }

    /// An attempt that saw almost nothing: no stop, no model answer, no session,
    /// no commit, no usage and no gate. Every optional half of a record is the
    /// report's to say something about.
    fn quiet() -> AttemptRecord {
        AttemptRecord {
            id: AttemptId::new(3),
            task: TaskId::new(7),
            started: datetime!(2026-09-20 11:00:00 UTC),
            ended: None,
            model_configured: None,
            model_reported: None,
            session_id: None,
            exit_reason: "the process was found dead".to_owned(),
            gates: Vec::new(),
            usage: None,
            base_sha: BASE.to_owned(),
            candidate_sha: None,
        }
    }

    #[test]
    fn the_context_an_attempt_was_given_is_stored_as_it_was_given() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());

        write_evidence(&project, &record(), CONTEXT).expect("an attempt writes its evidence");

        assert_eq!(
            artifact(&project, 7, 2, "context.md"),
            CONTEXT,
            "the context an attempt was given is the context its evidence shows: the blank \
             line inside it and the newline ending it are part of what it was told"
        );

        let brief = "one line, with no newline at the end of it";
        write_evidence(&project, &numbered(3), brief).expect("a later attempt files its own");
        assert_eq!(
            artifact(&project, 7, 3, "context.md"),
            brief,
            "a context that stops mid-line is stored stopping mid-line: inventing a newline \
             would store something the attempt was never given"
        );
    }

    #[test]
    fn the_record_beside_the_report_is_the_record_the_journal_holds() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());

        write_evidence(&project, &record(), CONTEXT).expect("an attempt writes its evidence");

        let text = artifact(&project, 7, 2, "record.json");
        let object: Value = serde_json::from_str(&text).expect("`record.json` is a JSON object");
        let mut carried: Vec<&str> = object
            .as_object()
            .expect("a record is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        carried.sort_unstable();
        let mut documented = FIELDS.to_vec();
        documented.sort_unstable();
        assert_eq!(
            carried, documented,
            "the stored record carries the fields the shape fixes and no others, so a reader \
             of the directory and a reader of the journal are reading one shape"
        );
        assert_eq!(
            text.matches('\n').count(),
            1,
            "one record is one line ending the file: a reader that has to know where a \
             record stops is a reader of a torn one"
        );

        let held: AttemptRecord =
            serde_json::from_str(&text).expect("`record.json` reads back as an attempt record");
        assert_eq!(
            held,
            record(),
            "what was written to disk is the record itself, not a summary of it"
        );
    }

    #[test]
    fn the_report_says_what_the_attempt_knew() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());

        write_evidence(&project, &record(), CONTEXT).expect("an attempt writes its evidence");

        assert_eq!(
            artifact(&project, 7, 2, "report.md"),
            [
                "# task 7 attempt 2",
                "",
                "- started: 2026-09-20T09:14:03.5Z",
                "- ended: 2026-09-20T09:41:47Z",
                "- model configured: gpt-5.6-sol",
                "- model reported: gpt-5.6-sol-2026-09-01",
                "- session: sess_01HQZK",
                "- exit reason: gate verify failed: 2 tests refused",
                "- base commit: 0b78d3f1c2a4",
                "- candidate commit: b7d1f3a9e5c2",
                "- usage: in=12000 out=3400 cached=9000 cost=$0.42",
                "- gates: verify refused (exit 101, 41930ms); format passed (exit 0, 940ms)",
                "",
            ]
            .join("\n"),
            "the report is the whole record in the words an operator reads: both model \
             witnesses, both commits, the gates in the order they ran, and the two instants \
             in the one spelling the journal uses"
        );
    }

    #[test]
    fn the_report_writes_what_an_attempt_never_saw_as_a_word_rather_than_as_a_zero() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());

        write_evidence(&project, &quiet(), CONTEXT).expect("an attempt writes its evidence");

        let text = artifact(&project, 7, 3, "report.md");
        assert_eq!(
            text,
            [
                "# task 7 attempt 3",
                "",
                "- started: 2026-09-20T11:00:00Z",
                "- ended: no stop reported",
                "- model configured: none",
                "- model reported: none",
                "- session: none",
                "- exit reason: the process was found dead",
                "- base commit: 0b78d3f1c2a4",
                "- candidate commit: none",
                "- usage: unasked",
                "- gates: none",
                "",
            ]
            .join("\n"),
            "an attempt that reported nothing says so in words: ADR-0049 refused a zero for \
             a fact that was never observed, and a report is where a human reads it"
        );
        for invented in ["null", "exit 0", "$0", "0ms"] {
            assert!(
                !text.contains(invented),
                "the report wrote {invented} for a fact this attempt never observed"
            );
        }
    }

    #[test]
    fn the_report_tells_a_session_that_reported_nothing_from_one_there_never_was() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());

        write_evidence(&project, &first_attempt(), CONTEXT)
            .expect("the first attempt writes its evidence");
        write_evidence(&project, &quiet(), CONTEXT).expect("the third does the same");

        let asked = artifact(&project, 7, 1, "report.md");
        let unasked = artifact(&project, 7, 3, "report.md");
        assert!(
            asked.contains("- usage: unreported"),
            "an attempt with a session that reported no figures is reported as unreported: \
             {asked}"
        );
        assert!(
            unasked.contains("- usage: unasked"),
            "an attempt with no session at all is a different fact: {unasked}"
        );
    }

    #[test]
    fn a_gate_log_says_how_the_gate_stopped_and_then_what_it_wrote() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());

        write_evidence(&project, &record(), CONTEXT).expect("an attempt writes its evidence");

        assert_eq!(
            artifact(&project, 7, 2, "gates/verify.log"),
            [
                "gate verify refused (exit 101, 41930ms)",
                "test attempt::tests::records .. FAILED",
                "",
            ]
            .join("\n"),
            "a gate's log says which gate it is, whether it passed, and how it stopped, and \
             then holds its output as it was written"
        );
        assert_eq!(
            artifact(&project, 7, 2, "gates/format.log"),
            [
                "gate format passed (exit 0, 940ms)",
                "-- stderr --",
                "Diff in src/attempt.rs at line 12:",
                "",
            ]
            .join("\n"),
            "output that came only on standard error is labelled as such, because nothing \
             else in the file says which half of the command's output it is"
        );
    }

    #[test]
    fn a_gate_stopped_by_a_timeout_or_a_signal_is_logged_as_that_stop() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        let killed = GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: Some(137),
            signal: Some(9),
            duration_ms: 30_000,
            stdout: "killed\n".to_owned(),
            stderr: String::new(),
            timed_out: true,
        };
        let segfaulted = GateResult {
            kind: GateKind::Lint,
            passed: false,
            exit_code: None,
            signal: Some(11),
            duration_ms: 4_200,
            stdout: "segmentation fault".to_owned(),
            stderr: String::new(),
            timed_out: false,
        };
        let silent = GateResult {
            kind: GateKind::Build,
            passed: false,
            exit_code: None,
            signal: None,
            duration_ms: 1_500,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        };
        let mut stopped = record();
        stopped.gates = vec![killed, segfaulted, silent];

        write_evidence(&project, &stopped, CONTEXT)
            .expect("an attempt whose gates died in three ways files its evidence");

        for (name, expected) in [
            (
                "gates/verify.log",
                "gate verify refused (timed out, 30000ms)\nkilled\n",
            ),
            (
                "gates/lint.log",
                "gate lint refused (signal 11, 4200ms)\nsegmentation fault\n",
            ),
            (
                "gates/build.log",
                "gate build refused (stopped without a verdict, 1500ms)\n",
            ),
        ] {
            assert_eq!(
                artifact(&project, 7, 2, name),
                expected,
                "a timeout is the fact a gate that was killed for one stopped on, and a \
                 signal leaves no exit code behind; a gate that wrote neither half leaves \
                 the verdict line alone"
            );
        }
    }

    #[test]
    fn a_gate_that_ran_twice_in_one_attempt_keeps_both_runs() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        let red = GateResult {
            kind: GateKind::Targeted,
            passed: false,
            exit_code: Some(101),
            signal: None,
            duration_ms: 1_200,
            stdout: "test tests::red .. FAILED\n".to_owned(),
            stderr: String::new(),
            timed_out: false,
        };
        let green = GateResult {
            kind: GateKind::Targeted,
            passed: true,
            exit_code: Some(0),
            signal: None,
            duration_ms: 810,
            stdout: "test tests::green .. ok\n".to_owned(),
            stderr: String::new(),
            timed_out: false,
        };
        let mut twice = record();
        twice.gates = vec![red, green];

        write_evidence(&project, &twice, CONTEXT)
            .expect("a protocol that loops its targeted gate files both runs");

        assert_eq!(
            tree(&project.state_dir.join("attempts")),
            [
                "7/",
                "7/2/",
                "7/2/context.md",
                "7/2/gates/",
                "7/2/gates/targeted.log",
                "7/2/record.json",
                "7/2/report.md",
            ],
            "a kind is a gate's name, not one of its runs: the log is named for the kind"
        );
        assert_eq!(
            artifact(&project, 7, 2, "gates/targeted.log"),
            [
                "gate targeted refused (exit 101, 1200ms)",
                "test tests::red .. FAILED",
                "",
                "gate targeted passed (exit 0, 810ms)",
                "test tests::green .. ok",
                "",
            ]
            .join("\n"),
            "VISION.md §9 makes RED and GREEN evidence of one attempt evidence of the same \
             attempt, so the second run is added beside the first rather than replacing it"
        );
        assert!(
            artifact(&project, 7, 2, "report.md").contains(
                "- gates: targeted refused (exit 101, 1200ms); targeted passed (exit 0, 810ms)"
            ),
            "the report lists the runs in the order they ran, two of them, or a reader of \
             the report alone cannot tell that the gate was re-run"
        );
    }

    /// Put `held`'s record at `dir/record.json` by hand. A writer files a record
    /// where its own fields name, so hand-placing one is the only way to make the
    /// evidence disagree with the directory holding it.
    fn place(dir: &Path, held: &AttemptRecord) {
        fs::create_dir_all(dir).expect("an evidence directory can be made by hand");
        let text = serde_json::to_string(held).expect("an attempt record encodes as text");
        fs::write(dir.join("record.json"), format!("{text}\n"))
            .expect("a record can be written by hand");
    }

    #[test]
    fn a_task_that_wrote_no_evidence_reads_back_as_no_attempts() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());

        let read = read_evidence(&project, TaskId::new(7))
            .expect("a task that never ran has no evidence, which is not a failure to read");

        assert_eq!(
            read,
            Vec::<AttemptRecord>::new(),
            "no evidence is an empty answer"
        );
        assert!(
            !project.state_dir.join("attempts").exists(),
            "reading an absent layout does not create one of its own"
        );
    }

    #[test]
    fn attempts_read_back_in_attempt_order_whatever_order_the_directories_were_written_in() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());

        for attempt in [3, 1, 2] {
            write_evidence(&project, &numbered(attempt), CONTEXT)
                .expect("an attempt files its own evidence");
        }

        let read = read_evidence(&project, TaskId::new(7)).expect("three attempts read back");
        assert_eq!(
            read.iter().map(|held| held.id).collect::<Vec<_>>(),
            vec![AttemptId::new(1), AttemptId::new(2), AttemptId::new(3)],
            "the order a reader comparing one attempt with the next wants is the order the \
             attempts ran, whatever order the writes reached the filesystem in"
        );
        assert_eq!(
            read[1],
            numbered(2),
            "each attempt's own evidence is what comes back, not the last one written"
        );
    }

    #[test]
    fn an_attempt_directory_holding_no_record_is_refused_as_damage() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        let dir = attempt_dir(&project, 7, 2);
        fs::create_dir_all(dir.join("gates"))
            .expect("an interrupted write leaves a directory and its logs behind");

        let error = read_evidence(&project, TaskId::new(7))
            .expect_err("a directory with no record is not an attempt that never ran");

        assert!(
            matches!(error, Error::Corrupt { seq: None, .. }),
            "the evidence directory is not the journal, so a damaged one is refused without \
             a sequence to point at: {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains(&dir.display().to_string()),
            "the refusal names the directory a repair starts from: {message}"
        );
        assert!(
            message.ends_with(
                "holds no `record.json`, which its writer writes last, so the \
                              write was interrupted"
            ),
            "{message}"
        );
    }

    #[test]
    fn a_record_that_cannot_be_read_back_is_refused_as_damage() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        write_evidence(&project, &record(), CONTEXT).expect("an attempt writes its evidence");
        fs::write(
            attempt_dir(&project, 7, 2).join("record.json"),
            "{\"id\": 2, \"task\":\n",
        )
        .expect("an interrupted write leaves half a record");

        let error = read_evidence(&project, TaskId::new(7))
            .expect_err("half a record is not an attempt record");
        let message = error.to_string();

        assert!(
            message.contains("record.json") && message.contains("cannot be read back"),
            "the refusal says which file failed to read: {message}"
        );
    }

    #[test]
    fn an_entry_that_is_not_an_attempt_directory_is_refused_as_damage() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        let notes = project
            .state_dir
            .join("attempts")
            .join("7")
            .join("notes.txt");
        fs::create_dir_all(notes.parent().expect("a directory path has a parent"))
            .expect("a task's evidence directory");
        fs::write(&notes, "not an attempt\n").expect("a stray file sits beside the attempts");

        let error = read_evidence(&project, TaskId::new(7))
            .expect_err("an entry that is not an attempt directory is not skipped");
        let message = error.to_string();
        assert!(
            message.contains("notes.txt")
                && message.ends_with(
                    "sits in the evidence directory of task 7, which holds one \
                               directory per attempt named by its number"
                ),
            "{message}"
        );

        fs::remove_file(&notes).expect("the stray file can be removed");
        fs::write(attempt_dir(&project, 7, 3), "not a directory\n")
            .expect("an attempt number that turned out to be a file");
        let message = read_evidence(&project, TaskId::new(7))
            .expect_err("a file named like an attempt is not an attempt")
            .to_string();
        assert!(
            message.contains("attempts/7/3") && message.contains("evidence directory of task"),
            "{message}"
        );
    }

    #[test]
    fn evidence_naming_an_attempt_or_a_task_it_was_not_read_for_is_refused() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        let misfiled = AttemptRecord {
            task: TaskId::new(8),
            ..record()
        };
        place(&attempt_dir(&project, 7, 2), &misfiled);

        let message = read_evidence(&project, TaskId::new(7))
            .expect_err("evidence about another task cannot be read as evidence about this one")
            .to_string();
        assert!(
            message.ends_with("holds attempt 2 of task 8, not attempt 2 of task 7"),
            "the refusal says which of the two the directory claims: {message}"
        );

        fs::remove_dir_all(attempt_dir(&project, 7, 2)).expect("the first case is cleared away");
        place(&attempt_dir(&project, 7, 5), &record());
        let message = read_evidence(&project, TaskId::new(7))
            .expect_err("a record cannot be filed under an attempt number it does not carry")
            .to_string();
        assert!(
            message.ends_with("holds attempt 2 of task 7, not attempt 5 of task 7"),
            "{message}"
        );
    }

    #[test]
    fn writing_one_attempts_evidence_twice_changes_nothing() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        write_evidence(&project, &record(), CONTEXT).expect("an attempt writes its evidence");
        let kept = evidence(&project, 7, 2);
        let kept_tree = tree(&project.state_dir.join("attempts"));

        write_evidence(&project, &record(), CONTEXT)
            .expect("filing one attempt again is a repair, not a second attempt");

        assert_eq!(
            evidence(&project, 7, 2),
            kept,
            "the same attempt writes the same bytes: evidence a retry rewrote would be \
             evidence the retry produced"
        );
        assert_eq!(kept_tree, tree(&project.state_dir.join("attempts")));
        assert_eq!(
            read_evidence(&project, TaskId::new(7)).expect("one attempt reads back"),
            vec![record()],
            "and it is still one attempt, not two"
        );
    }

    #[test]
    fn an_attempt_that_already_has_evidence_refuses_a_different_record() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        let other = tempdir().expect("a second scratch directory beside the repository");
        let untouched = registered(other.path());
        write_evidence(&project, &record(), CONTEXT).expect("an attempt writes its evidence");
        let kept = evidence(&project, 7, 2);
        let mut contradicted = record();
        contradicted.exit_reason = "the retry's own words".to_owned();

        let error = write_evidence(&project, &contradicted, "a different context")
            .expect_err("one attempt cannot answer twice");
        let message = format!("{error:?}");
        let Error::Policy { detail, paths } = error else {
            panic!("a second record for one attempt is a policy refusal: {message}");
        };
        assert_eq!(
            paths,
            vec![attempt_dir(&project, 7, 2)],
            "the refusal names the directory that is already spoken for"
        );
        assert!(
            detail.ends_with(
                "; one attempt writes one record, and the one already there says \
                            something different"
            ),
            "{detail}"
        );
        assert_eq!(
            evidence(&project, 7, 2),
            kept,
            "a refusal changes nothing that is already on disk"
        );

        let held = attempt_dir(&untouched, 7, 2);
        place(&held, &record());
        write_evidence(&untouched, &contradicted, "a different context")
            .expect_err("the refusal comes before the writer makes anything");
        assert_eq!(
            tree(&untouched.state_dir.join("attempts")),
            ["7/", "7/2/", "7/2/record.json"],
            "a refusal leaves the directory exactly as it found it, so a repair can tell a \
             refused write from a completed one"
        );
    }

    #[test]
    fn an_attempt_directory_left_by_an_interrupted_write_is_written_whole_again() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        write_evidence(&project, &record(), CONTEXT).expect("an attempt writes its evidence");
        let held = attempt_dir(&project, 7, 2);
        fs::write(held.join("record.json"), "{\"id\": 2,").expect("a write stops mid-record");
        fs::write(
            held.join("gates").join("stale.log"),
            "from an older shape\n",
        )
        .expect("a file an older write left is there to be found");

        write_evidence(&project, &record(), CONTEXT)
            .expect("a half-written attempt directory is written whole again");

        assert_eq!(
            tree(&project.state_dir.join("attempts")),
            LAYOUT,
            "the directory left by an interrupted write is rewritten, not merged into: a \
             stale log beside a new record would be evidence about a run nobody has"
        );
        let written = artifact(&project, 7, 2, "record.json");
        assert!(
            written.starts_with("{\"id\":2,\"task\":7,") && written.ends_with("}\n"),
            "the record the interrupted write never finished is there now: {written}"
        );
        assert_eq!(
            read_evidence(&project, TaskId::new(7)).expect("the repaired attempt reads back"),
            vec![record()]
        );
        for name in ["", "7", "7/2", "7/2/gates"] {
            assert_eq!(
                mode(&project.state_dir.join("attempts").join(name)),
                DIR_MODE,
                "a repaired directory is as closed as a newly written one"
            );
        }
    }

    #[test]
    fn evidence_below_a_state_directory_that_is_not_there_is_refused() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let id = "0123456789abcdef".to_owned();
        let missing = scratch.path().join("state").join(&id);
        let unregistered = Project {
            root: scratch.path().join("repository"),
            id,
            state_dir: missing.clone(),
        };

        let error = write_evidence(&unregistered, &record(), CONTEXT)
            .expect_err("an unregistered project has no evidence home to write to");
        let message = format!("{error:?}");
        let Error::NotFound { what } = error else {
            panic!("a state directory that is not there is not found: {message}");
        };
        assert!(
            what.starts_with("state directory of project 0123456789abcdef (")
                && what.ends_with(')'),
            "the refusal names the project and the directory it looked in: {what}"
        );
        assert!(
            !missing.exists(),
            "a refused write does not create the state directory it was refused for"
        );
    }

    #[test]
    fn an_evidence_path_occupied_by_a_file_or_a_link_is_refused() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        let attempts = project.state_dir.join("attempts");
        let occupied = attempts.join("7");
        fs::create_dir_all(&attempts).expect("the attempts directory itself");
        fs::write(&occupied, "not a directory\n").expect("a file sits where task 7 goes");

        let error = write_evidence(&project, &record(), CONTEXT)
            .expect_err("a file where a task's evidence goes is refused, not deleted");
        let message = format!("{error:?}");
        let Error::Policy { detail, paths } = error else {
            panic!("an occupied evidence path is a policy refusal: {message}");
        };
        assert_eq!(
            paths,
            vec![occupied.clone()],
            "the refusal names the path it refused to replace"
        );
        assert!(
            detail.ends_with("is already there and is not a directory"),
            "{detail}"
        );
        assert_eq!(
            fs::read_to_string(&occupied).expect("the file the writer refused to touch"),
            "not a directory\n"
        );

        fs::remove_file(&occupied).expect("the file is removed for the second case");
        let elsewhere = scratch.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).expect("a directory somewhere else");
        symlink(&elsewhere, &occupied).expect("a link sits where task 7's evidence goes");
        let error = write_evidence(&project, &record(), CONTEXT)
            .expect_err("a link is not a directory the supervisor owns");
        assert!(
            matches!(error, Error::Policy { .. }),
            "evidence must not be written through a link somebody else placed: {error:?}"
        );
        assert!(
            !elsewhere.join("2").exists(),
            "nothing was written through the link"
        );
    }

    /// A path whose permissions one test took away, handed back when it ends.
    ///
    /// The restore lives in [`Drop`] rather than at the end of the test body,
    /// because a failing assertion unwinds past the end: a scratch directory left
    /// at `0o000` cannot be removed, so the next test would trip over what this
    /// one left behind and be blamed for it.
    struct Sealed {
        path: PathBuf,
        before: u32,
    }

    impl Drop for Sealed {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(self.before));
        }
    }

    /// Take `path` down to `wanted`, remembering what it was so it goes back.
    fn sealed(path: &Path, wanted: u32) -> Sealed {
        let before = mode(path);
        fs::set_permissions(path, fs::Permissions::from_mode(wanted)).unwrap_or_else(|why| {
            panic!("`{}` should take mode {wanted:o}: {why}", path.display())
        });
        Sealed {
            path: path.to_path_buf(),
            before,
        }
    }

    #[test]
    fn evidence_below_a_state_directory_occupied_by_a_file_is_refused() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let id = "0123456789abcdef".to_owned();
        let state_dir = scratch.path().join("state").join(&id);
        fs::create_dir_all(state_dir.parent().expect("a state directory has a parent"))
            .expect("a scratch application directory");
        fs::write(&state_dir, "not a directory\n")
            .expect("a file sits where a project's state directory belongs");
        let project = Project {
            root: scratch.path().join("repository"),
            id,
            state_dir: state_dir.clone(),
        };

        let error = write_evidence(&project, &record(), CONTEXT)
            .expect_err("a file is not a directory a task's evidence can live below");
        let message = format!("{error:?}");
        let Error::Policy { detail, paths } = error else {
            panic!("a state directory occupied by a file is a policy refusal: {message}");
        };
        assert_eq!(
            paths,
            vec![state_dir.clone()],
            "the refusal names the file it refused to replace"
        );
        assert!(
            detail.ends_with("is already there and is not a directory"),
            "{detail}"
        );
        assert_eq!(
            fs::read_to_string(&state_dir).expect("the file the writer refused to touch"),
            "not a directory\n",
            "a refusal about a path it cannot write must not have written over it"
        );
    }

    #[test]
    fn a_state_directory_the_filesystem_will_not_let_us_look_at_is_not_reported_absent() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        let app_dir = project
            .state_dir
            .parent()
            .expect("a state directory is below an application directory")
            .to_path_buf();
        let _sealed = sealed(&app_dir, 0o000);

        let error = write_evidence(&project, &record(), CONTEXT)
            .expect_err("a directory we cannot look inside is not a project never registered");
        assert!(
            matches!(&error, Error::Io(why) if why.kind() == ErrorKind::PermissionDenied),
            "the filesystem's own refusal is the answer: a NotFound would send the operator \
             to register the project again, which is not the thing that went wrong: {error:?}"
        );
    }

    #[test]
    fn an_attempt_directory_we_cannot_list_is_not_read_as_a_task_that_never_ran() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        write_evidence(&project, &record(), CONTEXT).expect("an attempt writes its evidence");
        let task_dir = project.state_dir.join("attempts").join("7");
        let _sealed = sealed(&task_dir, 0o000);

        let error = read_evidence(&project, TaskId::new(7))
            .expect_err("an attempt directory we cannot list is not a task with no attempts");
        assert!(
            matches!(&error, Error::Io(why) if why.kind() == ErrorKind::PermissionDenied),
            "a task whose evidence exists but cannot be listed must not answer as empty, \
             because an empty answer is the one that hides an attempt that ran: {error:?}"
        );
    }

    #[test]
    fn a_record_the_filesystem_will_not_let_us_read_is_refused_rather_than_answered() {
        let scratch = tempdir().expect("a scratch directory beside the repository");
        let project = registered(scratch.path());
        let held = record();
        write_evidence(&project, &held, CONTEXT).expect("an attempt writes its evidence");
        let filed = attempt_dir(&project, 7, 2).join("record.json");
        let bytes = fs::read(&filed).expect("the record a write just filed");

        {
            let _sealed = sealed(&filed, 0o200);

            let error = write_evidence(&project, &held, CONTEXT)
                .expect_err("a record we cannot open is not a record that is not there");
            assert!(
                matches!(&error, Error::Io(why) if why.kind() == ErrorKind::PermissionDenied),
                "a write that cannot check what an attempt already holds has to refuse it, \
                 since replacing a record it never read is how an attempt answers twice: \
                 {error:?}"
            );

            let read = read_evidence(&project, TaskId::new(7))
                .expect_err("a record we cannot open is not a record that is damaged");
            assert!(
                matches!(&read, Error::Io(why) if why.kind() == ErrorKind::PermissionDenied),
                "the filesystem's refusal is not the answer `Corrupt`, which asks for a \
                 repair that would delete a file holding its evidence: {read:?}"
            );
        }

        assert_eq!(
            fs::read(&filed).expect("the record is readable again"),
            bytes,
            "neither refusal wrote to the file it could not read"
        );
    }
}
