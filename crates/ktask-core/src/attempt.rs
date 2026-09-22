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

use crate::gate::GateResult;
use crate::ids::{AttemptId, TaskId};
use crate::provider::Usage;
use serde::{Deserialize, Serialize};
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
}
