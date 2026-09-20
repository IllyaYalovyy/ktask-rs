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

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{AttemptId, Error, EventKind, GateResult, Journal, Result, TaskId, Usage};

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

#[cfg(test)]
mod tests {
    use super::AttemptRecord;
    use crate::{
        AttemptId, Error, EventKind, GateKind, GateResult, Journal, PauseReason, TaskId, Usage,
        UsageSource, attempt_records, journal_path,
    };
    use serde_json::Value;
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
}
