//! `AttemptRecord`: the durable evidence trail for one execution attempt at
//! a task.
//!
//! `VISION.md` §6: "Every attempt is preserved separately: executor session
//! ID, timestamps, configured and provider-reported model IDs, exit reason,
//! commands run, gate results, git SHAs, tokens, and cost." Recorded through
//! `EventKind::AttemptRecorded`, an `AttemptRecord` is appended to the
//! journal like any other event — a retry never overwrites a prior attempt's
//! record, it adds its own, and the journal's own append-only guarantee
//! (`journal.rs`) is what keeps every attempt's evidence intact.

use crate::Result;
use crate::gate::{GateKind, GateResult};
use crate::ids::{AttemptId, TaskId};
use crate::project::Project;
use crate::provider::Usage;
use crate::redact::redact;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// One execution attempt's durable evidence: what ran, what it reported,
/// what the gates found, and where it left the repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptRecord {
    /// This attempt's position within its task.
    pub id: AttemptId,
    /// The task this attempt belongs to.
    pub task: TaskId,
    /// When the attempt began.
    pub started: OffsetDateTime,
    /// When the attempt ended, if it has.
    pub ended: Option<OffsetDateTime>,
    /// The model configured for this attempt, if any.
    pub model_configured: Option<String>,
    /// The model the provider actually reported using, if any.
    pub model_reported: Option<String>,
    /// The executor's session id, if the provider assigned one.
    pub session_id: Option<String>,
    /// Why the attempt ended.
    pub exit_reason: String,
    /// The mechanical quality gates run against this attempt's result.
    pub gates: Vec<GateResult>,
    /// Token and cost usage, if known.
    pub usage: Option<Usage>,
    /// The commit this attempt started from.
    pub base_sha: String,
    /// The commit this attempt produced, if it produced one.
    pub candidate_sha: Option<String>,
}

/// This attempt's evidence directory: `<state_dir>/attempts/<task>/<attempt>`.
///
/// Keyed by both ids, not just the attempt's, since [`AttemptId`] only counts
/// within its own task (VISION.md §6, `docs/CONTRACT.md`): two different
/// tasks' first attempts would otherwise collide on the same directory name.
fn attempt_dir(project: &Project, task: TaskId, attempt: AttemptId) -> PathBuf {
    project
        .state_dir
        .join("attempts")
        .join(task.get().to_string())
        .join(attempt.get().to_string())
}

/// Restricts `dir` to owner-only access. A no-op on non-Unix targets, since
/// there is no equivalent mode bit to set.
#[cfg(unix)]
fn set_private(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private(_dir: &Path) -> Result<()> {
    Ok(())
}

/// The file name [`write_evidence`] gives a gate's captured output under
/// `gates/`: the gate's kind, lowercased (`Verify` -> `verify.log`).
fn gate_log_name(kind: GateKind) -> String {
    format!("{kind:?}").to_lowercase()
}

/// Returns a copy of `record` with every string that might carry a leaked
/// credential passed through [`redact`] — gate output first and foremost,
/// but also the free-text fields a provider or the runner itself fills in,
/// none of which are trusted to be clean.
fn redacted(record: &AttemptRecord) -> AttemptRecord {
    let mut copy = record.clone();
    copy.exit_reason = redact(&copy.exit_reason, &[]);
    copy.model_configured = copy.model_configured.map(|value| redact(&value, &[]));
    copy.model_reported = copy.model_reported.map(|value| redact(&value, &[]));
    copy.session_id = copy.session_id.map(|value| redact(&value, &[]));
    copy.base_sha = redact(&copy.base_sha, &[]);
    copy.candidate_sha = copy.candidate_sha.map(|value| redact(&value, &[]));
    for gate in &mut copy.gates {
        gate.stdout = redact(&gate.stdout, &[]);
        gate.stderr = redact(&gate.stderr, &[]);
    }
    copy
}

/// Renders `record` (already [`redacted`]) as the short human-readable
/// summary [`write_evidence`] writes to `report.md`.
fn render_report(record: &AttemptRecord) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Attempt {} for task {}", record.id, record.task);
    let _ = writeln!(out, "- started: {}", record.started);
    if let Some(ended) = record.ended {
        let _ = writeln!(out, "- ended: {ended}");
    }
    let _ = writeln!(out, "- exit_reason: {}", record.exit_reason);
    if let Some(model) = &record.model_configured {
        let _ = writeln!(out, "- model_configured: {model}");
    }
    if let Some(model) = &record.model_reported {
        let _ = writeln!(out, "- model_reported: {model}");
    }
    if let Some(session_id) = &record.session_id {
        let _ = writeln!(out, "- session_id: {session_id}");
    }
    let _ = writeln!(out, "- base_sha: {}", record.base_sha);
    if let Some(candidate_sha) = &record.candidate_sha {
        let _ = writeln!(out, "- candidate_sha: {candidate_sha}");
    }

    out.push_str("\n## Gates\n");
    if record.gates.is_empty() {
        out.push_str("(none)\n");
    }
    for gate in &record.gates {
        let _ = writeln!(
            out,
            "- {:?}: {}",
            gate.kind,
            if gate.passed { "passed" } else { "failed" }
        );
    }
    out
}

/// Writes `record`'s evidence to its own directory under `project`'s state
/// directory, per VISION.md §6/§11: `<state_dir>/attempts/<task>/<attempt>/`
/// holding `report.md` (a rendered summary), `context.md` (`context`, as
/// given to the agent for this attempt), `gates/<kind>.log` (one file per
/// gate `record` carries a result for) and `record.json` (`record` itself).
///
/// The directory — and its `gates/` subdirectory — is created with mode
/// `0700` on Unix, matching [`crate::Project::register`]'s state directory.
/// Every string written is passed through [`redact`] first, so a credential
/// that leaked into an agent's output, a gate's captured stdout/stderr, or
/// the assembled context never reaches disk.
///
/// Each attempt gets its own directory named for its [`AttemptId`]: calling
/// this again for a later attempt of the same task adds a sibling directory
/// rather than overwriting the first, so every attempt's evidence survives
/// every later one.
///
/// # Errors
///
/// Returns [`Error::Io`](crate::Error::Io) if the directory cannot be
/// created or a file cannot be written, and
/// [`Error::Serde`](crate::Error::Serde) if `record` cannot be serialized to
/// JSON.
pub fn write_evidence(project: &Project, record: &AttemptRecord, context: &str) -> Result<()> {
    let dir = attempt_dir(project, record.task, record.id);
    let gates_dir = dir.join("gates");
    fs::create_dir_all(&gates_dir)?;
    set_private(&dir)?;
    set_private(&gates_dir)?;

    let record = redacted(record);

    fs::write(dir.join("record.json"), serde_json::to_vec_pretty(&record)?)?;
    fs::write(dir.join("report.md"), render_report(&record))?;
    fs::write(dir.join("context.md"), redact(context, &[]))?;

    for gate in &record.gates {
        let combined = format!("{}{}", gate.stdout, gate.stderr);
        let name = format!("{}.log", gate_log_name(gate.kind));
        fs::write(gates_dir.join(name), combined)?;
    }

    Ok(())
}

/// Reads back every attempt recorded for `task` under `project`'s evidence
/// directory, oldest (lowest [`AttemptId`]) first.
///
/// Reads only `record.json` from each attempt directory — `report.md`,
/// `context.md` and `gates/*.log` are for a human or the TUI to inspect, not
/// for this to reconstruct a [`AttemptRecord`] from. Works from whatever is
/// on disk alone, independent of any in-memory state or the journal, so
/// evidence a previous process wrote is found the same way after a restart
/// or a rebuild of materialized state as it would be in the process that
/// wrote it. Returns an empty vec, not an error, when `task` has no
/// evidence directory yet.
///
/// # Errors
///
/// Returns [`Error::Io`](crate::Error::Io) if an attempt directory exists
/// but its `record.json` cannot be read, and
/// [`Error::Serde`](crate::Error::Serde) if it cannot be parsed.
pub fn read_evidence(project: &Project, task: TaskId) -> Result<Vec<AttemptRecord>> {
    let dir = project
        .state_dir
        .join("attempts")
        .join(task.get().to_string());
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut attempts: Vec<(u32, PathBuf)> = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(id) = name.parse::<u32>() else {
            continue;
        };
        attempts.push((id, path));
    }
    attempts.sort_by_key(|(id, _)| *id);

    attempts
        .into_iter()
        .map(|(_, path)| {
            let bytes = fs::read(path.join("record.json"))?;
            Ok(serde_json::from_slice(&bytes)?)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::GateKind;
    use crate::provider::UsageSource;
    use time::macros::datetime;

    fn sample(id: u32) -> AttemptRecord {
        AttemptRecord {
            id: AttemptId::new(id),
            task: TaskId::new(1),
            started: datetime!(2024-01-15 10:00:00 UTC),
            ended: Some(datetime!(2024-01-15 10:05:00 UTC)),
            model_configured: Some("claude-opus-4".to_string()),
            model_reported: Some("claude-opus-4-20250514".to_string()),
            session_id: Some("sess-123".to_string()),
            exit_reason: "completed".to_string(),
            gates: vec![GateResult {
                kind: GateKind::Verify,
                passed: true,
                exit_code: Some(0),
                signal: None,
                duration_ms: 42,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            }],
            usage: Some(Usage {
                input_tokens: Some(100),
                output_tokens: Some(50),
                cached_tokens: None,
                cost_usd: Some(0.05),
                source: UsageSource::Provider,
            }),
            base_sha: "base123".to_string(),
            candidate_sha: Some("cand456".to_string()),
        }
    }

    #[test]
    fn attempt_record_round_trips_through_json() {
        let record = sample(1);
        let json = serde_json::to_string(&record).expect("serialize");
        let back: AttemptRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(record, back);
    }

    #[test]
    fn attempt_record_with_every_optional_field_absent_round_trips() {
        let record = AttemptRecord {
            id: AttemptId::new(2),
            task: TaskId::new(3),
            started: datetime!(2024-01-15 10:00:00 UTC),
            ended: None,
            model_configured: None,
            model_reported: None,
            session_id: None,
            exit_reason: "interrupted".to_string(),
            gates: Vec::new(),
            usage: None,
            base_sha: "base789".to_string(),
            candidate_sha: None,
        };
        let json = serde_json::to_string(&record).expect("serialize");
        let back: AttemptRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(record, back);
    }

    #[test]
    fn attempt_record_id_and_task_distinguish_otherwise_identical_attempts() {
        let first = sample(1);
        let second = sample(2);
        assert_ne!(first, second);
        assert_eq!(first.task, second.task);
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn a_retry_appends_a_new_attempt_record_to_the_journal_instead_of_overwriting_the_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.db");
        let mut journal = crate::Journal::open(&path).expect("open");
        let task = TaskId::new(1);

        let first = sample(1);
        let second = sample(2);

        journal
            .append(
                Some(task),
                &crate::EventKind::AttemptRecorded {
                    record: Box::new(first.clone()),
                },
            )
            .expect("append first attempt record");
        journal
            .append(
                Some(task),
                &crate::EventKind::AttemptRecorded {
                    record: Box::new(second.clone()),
                },
            )
            .expect("append second attempt record");

        let records: Vec<AttemptRecord> = journal
            .events_for(task)
            .expect("events_for")
            .into_iter()
            .filter_map(|event| match event.kind {
                crate::EventKind::AttemptRecorded { record } => Some(*record),
                _ => None,
            })
            .collect();

        assert_eq!(
            records,
            vec![first, second],
            "both attempts must be preserved, in attempt order, not overwritten"
        );
    }

    fn project_at(state_dir: &Path) -> Project {
        Project {
            root: PathBuf::from("/repo"),
            id: "test-project".to_string(),
            state_dir: state_dir.to_path_buf(),
        }
    }

    #[test]
    fn write_evidence_creates_the_documented_layout() {
        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());
        let record = sample(1);

        write_evidence(&project, &record, "assembled context").expect("write_evidence");

        let dir = attempt_dir(&project, record.task, record.id);
        assert!(dir.join("record.json").is_file());
        assert!(dir.join("report.md").is_file());
        assert!(dir.join("context.md").is_file());
        assert!(dir.join("gates").join("verify.log").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn write_evidence_creates_directories_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());
        let record = sample(1);

        write_evidence(&project, &record, "context").expect("write_evidence");

        let dir = attempt_dir(&project, record.task, record.id);
        let mode = |p: &Path| fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("gates")), 0o700);
    }

    #[test]
    fn write_then_read_evidence_round_trips_a_single_attempt() {
        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());
        let record = sample(1);

        write_evidence(&project, &record, "context").expect("write_evidence");
        let read = read_evidence(&project, record.task).expect("read_evidence");

        assert_eq!(read, vec![record]);
    }

    #[test]
    fn a_retry_adds_a_sibling_directory_instead_of_overwriting_the_first() {
        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());
        let first = sample(1);
        let second = sample(2);

        write_evidence(&project, &first, "first context").expect("write first");
        write_evidence(&project, &second, "second context").expect("write second");

        let first_dir = attempt_dir(&project, first.task, first.id);
        let second_dir = attempt_dir(&project, second.task, second.id);
        assert_ne!(first_dir, second_dir);
        assert!(first_dir.is_dir());
        assert!(second_dir.is_dir());

        let read = read_evidence(&project, first.task).expect("read_evidence");
        assert_eq!(
            read,
            vec![first, second],
            "the first attempt's evidence must be intact, not overwritten by the retry"
        );
    }

    #[test]
    fn read_evidence_survives_a_rebuild_of_the_project_handle() {
        let state = tempfile::tempdir().expect("tempdir");
        let record = sample(1);

        {
            let project = project_at(state.path());
            write_evidence(&project, &record, "context").expect("write_evidence");
        }

        // A fresh `Project` value pointing at the same state directory
        // stands in for a rebuild of materialized state after a restart:
        // nothing about the process that wrote the evidence survives here.
        let rebuilt = project_at(state.path());
        let read = read_evidence(&rebuilt, record.task).expect("read_evidence");

        assert_eq!(read, vec![record]);
    }

    #[test]
    fn read_evidence_for_a_task_with_no_attempts_is_an_empty_vec_not_an_error() {
        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());

        let read = read_evidence(&project, TaskId::new(99)).expect("read_evidence");

        assert!(read.is_empty());
    }

    #[test]
    fn write_evidence_redacts_a_secret_out_of_gate_output_before_it_reaches_disk() {
        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());
        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
        let mut record = sample(1);
        record.gates[0].stdout = format!("leaked: {secret}");

        write_evidence(&project, &record, "context").expect("write_evidence");

        let dir = attempt_dir(&project, record.task, record.id);
        let log = fs::read_to_string(dir.join("gates").join("verify.log")).expect("read log");
        let json = fs::read_to_string(dir.join("record.json")).expect("read record.json");
        assert!(!log.contains(secret));
        assert!(log.contains("[redacted]"));
        assert!(!json.contains(secret));
    }

    #[test]
    fn write_evidence_redacts_a_secret_out_of_the_context() {
        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());
        let record = sample(1);
        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";

        write_evidence(&project, &record, &format!("context leaked: {secret}"))
            .expect("write_evidence");

        let dir = attempt_dir(&project, record.task, record.id);
        let context = fs::read_to_string(dir.join("context.md")).expect("read context.md");
        assert!(!context.contains(secret));
        assert!(context.contains("[redacted]"));
    }

    #[test]
    fn report_md_summarizes_the_attempts_identity_and_gate_outcomes() {
        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());
        let record = sample(1);

        write_evidence(&project, &record, "context").expect("write_evidence");

        let dir = attempt_dir(&project, record.task, record.id);
        let report = fs::read_to_string(dir.join("report.md")).expect("read report.md");
        assert!(report.contains("Attempt 1 for task 1"));
        assert!(report.contains("completed"));
        assert!(report.contains("Verify: passed"));
    }

    #[test]
    fn read_evidence_orders_attempts_by_ascending_attempt_id_regardless_of_write_order() {
        let state = tempfile::tempdir().expect("tempdir");
        let project = project_at(state.path());
        let first = sample(1);
        let second = sample(2);

        // Written out of order, to prove `read_evidence` sorts rather than
        // relying on directory iteration order.
        write_evidence(&project, &second, "second").expect("write second");
        write_evidence(&project, &first, "first").expect("write first");

        let read = read_evidence(&project, first.task).expect("read_evidence");

        assert_eq!(read, vec![first, second]);
    }
}
