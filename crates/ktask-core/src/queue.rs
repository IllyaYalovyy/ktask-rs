//! Which task the supervisor may start next, decided from queue state alone.
//!
//! VISION.md §3 makes the queue's order a promise rather than a preference: one
//! task is active at a time, and a successor cannot start until its predecessor
//! has been proved published. [`next_runnable`] is where the two checks that say
//! so are composed into the one question a screen, `run` or `resume` asks: *what
//! may start now?* It is pure — no journal, no clock, no lock — so the same queue
//! answers the same way before a task is started, after its transition has been
//! journaled, and in the replay that recovered a crash (VISION.md §5).
//!
//! Each rule is asked of the function that already owns it rather than restated
//! here, so that this module cannot drift from the invariants it composes:
//!
//! - [`check_one_active`] is asked of the projection as it stands. Two tasks
//!   active is not a queue with nothing to run: it is a projection claiming
//!   something no run could have done, so it is reported as the error it is.
//! - A run standing still, or stopped at a failure, has nothing to start.
//!   [`TaskState::is_paused`] and [`TaskState::Failed`] are asked of every row,
//!   because `run` stops at the first terminal failure and never walks past one
//!   (docs/CONTRACT.md).
//! - [`check_predecessor`] is asked of the id that is next in line. The states
//!   that clear its way are ADR-0028's list as ADR-0031 amended it, and where that
//!   list and the invariant's `is_terminal` states differ, the two ADRs say so.
//!
//! A gate is a queue entry that asks a human for something before the work after
//! it may proceed (VISION.md §6). It is never handed to an agent, so it is never
//! the answer, and the work behind it waits, so the answer is `Ok(None)` rather
//! than the task the gate is holding back. What marks a gate is its `**Gate:**`
//! section, not [`crate::TaskStatus::HumanGate`]: `task.rs` is explicit that
//! status is what a supervisor has concluded, and a conclusion is not what an
//! entry is.
//!
//! A gate stops holding the queue the moment a human passes it, because its row
//! then sits in `Acknowledged` — a terminal success the ordering check clears
//! (ADR-0031) and a state that is not awaiting a start, so the selector moves on
//! to the work the gate was holding. No gate is ever the answer for that reason:
//! acknowledging a gate closes it rather than making it runnable.
//!
//! [`load`] is this module's other half and the only part of it that touches a
//! file: it opens the project's journal and asks that for the rows. The two
//! halves stay apart on purpose. A selector that could read a journal for
//! itself would be free to answer differently the second time it was asked, and
//! what [`next_runnable`] is for is answering from the two inputs it was
//! handed.

use std::collections::BTreeMap;

use crate::error::Result;
use crate::ids::TaskId;
use crate::journal::Journal;
use crate::project::Project;
use crate::state::{TaskState, check_one_active, check_predecessor};
use crate::task::Task;

/// The queue of one registered project, read from the database that holds it.
///
/// There is no queue file to read and nothing to write back. A plan document is
/// an *input format*: [`crate::Journal::put_tasks`] wrote its rows once, into a
/// queue that held nothing, and from that moment the only place the queue lives
/// is the `tasks` table of the project's journal (`docs/DESIGN.md` Database
/// schema, VISION.md section 4). Nor is a task's status in those rows: it is the
/// [`crate::TaskState`] the events imply, held in the projection, so reading the
/// queue reports no status, marks nothing in progress, and leaves nothing
/// behind that has to be reconciled afterwards.
///
/// That is what makes the call cheap enough to make as often as a screen
/// redraws — one open and one read of one table, ordered by id, which *is*
/// document order — and what makes it correct at every moment a caller needs
/// it: before a task starts, after its transition has been journaled, and in
/// the replay that recovered a crash. It records no fact for recovery to have
/// to reason about, so it cannot be the half of a run that was interrupted.
///
/// A queue that holds nothing is an empty `Vec` rather than an error: that is
/// what every project starts with, what a drained one ends with, and the first
/// thing a run reads either way.
///
/// # Errors
///
/// Whatever [`crate::Journal::open_for`] and [`crate::Journal::tasks`] report.
/// In short: [`crate::Error::Database`] when the project's state directory or
/// its journal cannot be opened — a directory that was never registered is
/// refused rather than made, because one made here would look exactly like a
/// registration (VISION.md section 11); [`crate::Error::Config`] on a journal
/// written by a later schema version; [`crate::Error::Corrupt`] on a row, or a
/// version row, that this build cannot say a word about.
pub fn load(project: &Project) -> Result<Vec<Task>> {
    Journal::open_for(project)?.tasks()
}

/// The next task the queue may start, or `None` when nothing may start.
///
/// `tasks` is the queue — the rows [`crate::Journal::tasks`] reads back — and
/// `states` is the projection of the journal ([`crate::Journal::all_states`]).
/// Both are inputs a caller has already had to read, and nothing here reads a
/// third: the answer is a pure function of the pair, which is what makes one
/// call serve the drain's next step, the TUI queue screen's "what runs next",
/// and the question a recovery walk asks before it touches anything.
///
/// A task the projection holds no row for is read as [`TaskState::Queued`]. The
/// evidence that a task was queued is its row in the queue: an import writes
/// those rows and appends no event, so a freshly imported plan has no rows in
/// the projection at all, and a selector that refused to start its head would
/// make every plan unrunnable. The journal's own replay folds a task with no
/// events onto `Queued` for the same reason. A name that is in the
/// projection but not in the queue is not a task, and is never offered.
///
/// The first id still waiting decides the answer on its own, and nothing is
/// searched past it. A refusal to start it is a refusal to start every id below
/// it as well: the ordering refusal names a predecessor, and a predecessor of the
/// head is below everything; a gate holds the work after it (VISION.md §6); and
/// the task already in the active slot holds it from every candidate. So a
/// blocked queue answers in one step rather than after walking rows it cannot
/// start.
///
/// # Errors
///
/// [`crate::Error::Policy`] when the projection holds more than one active task,
/// which is [`check_one_active`]'s refusal passed through unchanged: a queue
/// with two runners is damage in the durable record, not a queue with nothing to
/// run, and the caller has to repair it rather than start a third task.
pub fn next_runnable(
    tasks: &[Task],
    states: &BTreeMap<TaskId, TaskState>,
) -> Result<Option<TaskId>> {
    // The queue has to be a queue before anything is chosen out of it, and a
    // projection with two runners is not one: it claims something no run could
    // have done. That is reported to whoever can repair it, not worked around by
    // starting a third task.
    check_one_active(states)?;

    // A run standing still, or stopped at a failure, has nothing to start. Both
    // are asked of every row rather than of the head, because both hold the whole
    // queue wherever they sit: a pause is released by `resume`, not by a successor
    // deciding to go ahead, and `run` never walks past a terminal failure
    // (docs/CONTRACT.md).
    if stands_still(states) {
        return Ok(None);
    }

    // Queue order is the ids, which the slice a caller read back need not be in.
    let mut queue: Vec<&Task> = tasks.iter().collect();
    queue.sort_unstable_by_key(|task| task.id);

    for task in queue {
        if !awaits_a_start(states, task.id) {
            continue;
        }
        // The first id still waiting answers for every id below it, so the search
        // stops here rather than walking rows this call cannot start.
        return Ok(startable(states, task));
    }
    Ok(None)
}

/// Whether the queue is standing still or stopped at a failure.
///
/// The five durable pauses of VISION.md §6 and the one failure a run stops at.
/// A [`TaskState::Cancelled`] task is deliberately absent: a human dropped it so
/// that the queue could proceed past it.
fn stands_still(states: &BTreeMap<TaskId, TaskState>) -> bool {
    states
        .values()
        .any(|state| state.is_paused() || matches!(state, TaskState::Failed { .. }))
}

/// Whether `task` is still waiting for its turn to start.
///
/// `Queued`, or no row at all — see [`next_runnable`] for why an import leaves a
/// plan with no rows to read.
fn awaits_a_start(states: &BTreeMap<TaskId, TaskState>, task: TaskId) -> bool {
    matches!(states.get(&task), None | Some(TaskState::Queued))
}

/// The answer about the one task that is next in line.
///
/// Every refusal here is an ordinary fact about a running queue — a person has
/// not been asked yet, the work below has not been published, something is
/// already running — so each is `None` rather than an error. The one state that
/// is not ordinary, two tasks active at once, was answered by `check_one_active`
/// before this was reached.
fn startable(states: &BTreeMap<TaskId, TaskState>, task: &Task) -> Option<TaskId> {
    if task.gate.is_some() {
        return None;
    }
    if check_predecessor(states, task.id).is_err() {
        return None;
    }
    if holds_the_slot(states, task.id) {
        return None;
    }
    Some(task.id)
}

/// Whether starting `candidate` would put a second task into the one active slot.
///
/// Asked of [`check_one_active`] over a projection in which the candidate has
/// been started — moved to [`TaskState::Preflight`], the state a
/// `PreflightStarted` record puts it in and the first one that occupies the slot
/// (ADR-0027) — rather than against a list of active states kept here. There is
/// then one answer to which states are active, and this cannot drift from it.
///
/// The copy is the point as much as the check: the candidate's own row says
/// `Queued`, which is not a state the slot is spoken of in, and the question is
/// what the queue looks like *after* the start the caller is considering.
/// ADR-0028 leaves `PublishedVerified` as the one state where the two rules
/// disagree — the ordering check clears it, ADR-0027 keeps it in the slot — and a
/// probe answers with the stricter of the two, which is the one invariant 1
/// admits.
fn holds_the_slot(states: &BTreeMap<TaskId, TaskState>, candidate: TaskId) -> bool {
    let mut started = states.clone();
    started.insert(candidate, TaskState::Preflight);
    check_one_active(&started).is_err()
}

#[cfg(test)]
mod tests {
    use super::next_runnable;
    use crate::state::{PauseReason, check_predecessor};
    use crate::task::{Task, TaskStatus, parse_plan};
    use crate::{AttemptId, Error, FailureClass, Phase, TaskId, TaskState};
    use crate::{EventKind, apply};
    use std::collections::BTreeMap;
    use std::fmt::Write;
    use time::OffsetDateTime;

    /// A plan of `count` tasks, parsed rather than assembled: a queue row is what
    /// [`parse_plan`] produces, and a selector tested against a hand-shaped `Task`
    /// would be tested against something the queue never holds.
    fn plan(count: usize, gated: &[usize]) -> Vec<Task> {
        parse_plan(&document(count, gated)).expect("the scratch plan is a plan the parser accepts")
    }

    /// `count` task blocks, each carrying the four required sections, plus a
    /// `**Gate:**` on the plan positions `gated` names.
    fn document(count: usize, gated: &[usize]) -> String {
        let mut text = String::new();
        for position in 1..=count {
            write!(
                text,
                "## Task {position}\n\n**Outcome:** what task {position} changes.\n\
                 **Done-when:** a test asserts it.\n\
                 **Verify:** `cargo nextest run -p ktask-core`\n\
                 **Refs:** VISION.md section 6\n"
            )
            .expect("a String always has room for what is written into it");
            if gated.contains(&position) {
                text.push_str("**Gate:** a person, not the supervisor, decides.\n");
            }
            text.push('\n');
        }
        text
    }

    /// A projection holding each `(position, state)` pair, in the shape the two
    /// checks take it.
    fn states(rows: &[(u32, TaskState)]) -> BTreeMap<TaskId, TaskState> {
        rows.iter()
            .map(|(position, state)| (TaskId::new(*position), state.clone()))
            .collect()
    }

    /// A task standing still for `reason`, holding `Queued` as where it comes
    /// back to.
    fn parked(reason: PauseReason) -> TaskState {
        TaskState::Paused {
            reason,
            resume_to: Box::new(TaskState::Queued),
        }
    }

    /// A task whose commit the remote was read back holding.
    fn published(commit: &str) -> TaskState {
        TaskState::PublishedVerified {
            commit: commit.to_owned(),
        }
    }

    /// A gate a human has passed.
    fn acknowledged() -> TaskState {
        TaskState::Acknowledged {
            by: "operator".to_owned(),
            at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// A terminal failure, which is what a run stops at.
    fn failed() -> TaskState {
        TaskState::Failed {
            class: FailureClass::EnvironmentFailure,
            detail: "the baseline command is not on PATH".to_owned(),
        }
    }

    /// The event a run journals when it reaches a gate: the queue stops and asks
    /// a person, which is the only pause `ack` can close.
    fn gate_paused() -> EventKind {
        EventKind::Paused {
            reason: PauseReason::HumanGate,
        }
    }

    /// The event `ktask-rs ack` journals. `by` and `at` are the pair VISION.md §3
    /// invariant 7 says a gate is recorded with, and the state helper above
    /// carries the same pair so the two can be compared.
    fn gate_acknowledged() -> EventKind {
        EventKind::GateAcknowledged {
            by: "operator".to_owned(),
            at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// The answer, spelled so a refusal and an empty answer are two different
    /// facts about the queue.
    #[derive(Debug, PartialEq)]
    enum Answer {
        /// The projection claims something no run could have done.
        Refused,
        /// Nothing may start.
        Nothing,
        /// Task `id` may start.
        Start(u32),
    }

    /// Asks `next_runnable` and spells its answer as [`Answer`]. Anything but
    /// the queue's own policy error is a bug, not an answer.
    fn answer(tasks: &[Task], projection: &BTreeMap<TaskId, TaskState>) -> Answer {
        match next_runnable(tasks, projection) {
            Ok(None) => Answer::Nothing,
            Ok(Some(id)) => Answer::Start(id.get()),
            Err(Error::Policy { .. }) => Answer::Refused,
            Err(other) => panic!("the queue answered with {other}, not its own rule"),
        }
    }

    #[test]
    fn a_freshly_imported_plan_offers_its_first_task() {
        // An import writes queue rows and appends no event, so the projection is
        // empty and every task is still `Queued`. Nothing may start but the head,
        // and if the head were refused no plan could ever be run.
        let tasks = plan(3, &[]);
        assert_eq!(
            answer(&tasks, &states(&[])),
            Answer::Start(1),
            "the head of an untouched plan is the only thing that can run"
        );
    }

    #[test]
    fn a_closed_predecessor_lets_the_next_task_start() {
        let tasks = plan(3, &[]);
        let projection = states(&[
            (1, TaskState::Done),
            (2, TaskState::Cancelled),
            (3, TaskState::Queued),
        ]);
        assert_eq!(
            answer(&tasks, &projection),
            Answer::Start(3),
            "work a human dropped is work the queue proceeds past, so task 3 is next"
        );
    }

    #[test]
    fn the_lowest_waiting_id_decides_whatever_order_the_rows_arrive_in() {
        // Queue order is the ids, not the order of the slice a caller happened to
        // read, so a rotated queue answers the same question.
        let mut tasks = plan(3, &[]);
        tasks.rotate_left(2);
        let projection = states(&[(1, TaskState::Done), (2, TaskState::Cancelled)]);
        assert_eq!(answer(&tasks, &projection), Answer::Start(3));
    }

    #[test]
    fn a_task_the_queue_does_not_hold_is_never_offered() {
        // The projection names a task 2 the queue has no row for. The queue is
        // what says which ids exist, so this is a drained queue rather than one
        // with a next task.
        let tasks = plan(1, &[]);
        let projection = states(&[(1, TaskState::Done), (2, TaskState::Queued)]);
        assert_eq!(
            answer(&tasks, &projection),
            Answer::Nothing,
            "a row in the projection with no row in the queue names no task"
        );
    }

    #[test]
    fn a_drained_queue_offers_nothing() {
        let tasks = plan(3, &[]);
        let projection = states(&[
            (1, TaskState::Done),
            (2, acknowledged()),
            (3, TaskState::Cancelled),
        ]);
        assert_eq!(
            answer(&tasks, &projection),
            Answer::Nothing,
            "a queue with nothing left in `Queued` has drained"
        );
    }

    #[test]
    fn a_queue_that_holds_nothing_offers_nothing() {
        assert_eq!(
            answer(&[], &states(&[])),
            Answer::Nothing,
            "a project's first queue is empty, which is drained rather than an error"
        );
    }

    #[test]
    fn a_failed_task_anywhere_in_the_queue_offers_nothing() {
        // Task 3 failed and task 1 is still waiting. The failure is above the
        // head, so no predecessor check would notice it — and the run still stops
        // all the same, because `run` never walks past a terminal failure
        // (docs/CONTRACT.md) and a successor of a failure is not started work.
        let tasks = plan(3, &[]);
        let projection = states(&[(1, TaskState::Queued), (3, failed())]);
        assert_eq!(
            answer(&tasks, &projection),
            Answer::Nothing,
            "a failure anywhere holds the whole queue, not only the ids below it"
        );
    }

    #[test]
    fn every_pause_reason_offers_nothing() {
        // A pause is the run standing still, whatever made it stand still: the
        // selector does not resume it, `ktask-rs resume` does.
        for reason in [
            PauseReason::Limit { until: None },
            PauseReason::Input,
            PauseReason::HumanGate,
            PauseReason::Interrupted,
            PauseReason::Blocked,
        ] {
            let tasks = plan(3, &[]);
            let projection = states(&[(1, TaskState::Queued), (3, parked(reason.clone()))]);
            assert_eq!(
                answer(&tasks, &projection),
                Answer::Nothing,
                "a task paused for {reason:?} holds the queue where it is"
            );
        }
    }

    #[test]
    fn a_pending_human_gate_offers_nothing() {
        let tasks = plan(2, &[1]);
        let projection = states(&[(1, TaskState::Queued), (2, TaskState::Queued)]);
        assert_eq!(
            answer(&tasks, &projection),
            Answer::Nothing,
            "a gate is never handed to an agent, so the queue waits on the person"
        );
    }

    #[test]
    fn a_gate_holds_back_the_work_after_it() {
        // Task 1 is closed and the gate is next in line. Offering task 3 would be
        // the gate bypass VISION.md §6 exists to prevent, and it is refused below
        // the gate as well as at it.
        let tasks = plan(3, &[2]);
        let projection = states(&[
            (1, TaskState::Done),
            (2, TaskState::Queued),
            (3, TaskState::Queued),
        ]);
        assert_eq!(
            answer(&tasks, &projection),
            Answer::Nothing,
            "the work after a gate waits for the gate"
        );
        assert!(
            check_predecessor(&projection, TaskId::new(3)).is_err(),
            "an unreached gate holds its successors by the ordering check as well"
        );
    }

    #[test]
    fn a_gate_the_queue_has_not_reached_holds_back_no_earlier_task() {
        // VISION.md §6 asks a gate for permission before the work *after* it may
        // proceed. Task 1 is before the gate, so the gate is not its business yet.
        let tasks = plan(2, &[2]);
        assert_eq!(answer(&tasks, &states(&[])), Answer::Start(1));
    }

    #[test]
    fn an_acknowledged_gate_lets_its_successors_start() {
        // The assertion ADR-0029 pointed at, and ADR-0031's answer to the question
        // ADR-0028 refused to answer alone: a gate produces no commit, so
        // `Acknowledged` is the only terminal success it can ever reach, and a
        // queue that would not proceed past it had one gate deadlock the rest of
        // itself (VISION.md §6).
        let tasks = plan(2, &[1]);
        let projection = states(&[(1, acknowledged()), (2, TaskState::Queued)]);
        assert_eq!(
            answer(&tasks, &projection),
            Answer::Start(2),
            "a gate a human has passed is work the queue proceeds past"
        );
        assert!(
            check_predecessor(&projection, TaskId::new(2)).is_ok(),
            "and it clears the ordering check, not only the selector"
        );
    }

    #[test]
    fn a_gate_a_run_stopped_at_still_holds_its_successors() {
        // The other side of the same line: an unpassed gate is not cleared work,
        // whether the run has not reached it, has stopped at it, or stopped beside
        // it for some other reason entirely.
        for gate in [
            TaskState::Queued,
            parked(PauseReason::HumanGate),
            parked(PauseReason::Input),
        ] {
            let projection = states(&[(1, gate.clone()), (2, TaskState::Queued)]);
            assert!(
                check_predecessor(&projection, TaskId::new(2)).is_err(),
                "a gate in {} has not been passed by anyone: it must hold task 2",
                gate.name()
            );
        }
    }

    #[test]
    fn a_gate_is_never_the_task_the_selector_names() {
        // VISION.md §6: a gate is never handed to an agent, so it never gets an
        // attempt. Asked of every state a queue row can be in, because the answer
        // must not depend on how far the run got before it reached the gate.
        let tasks = plan(3, &[2]);
        for gate in every_state() {
            let projection = states(&[
                (1, TaskState::Done),
                (2, gate.clone()),
                (3, TaskState::Queued),
            ]);
            if let Answer::Start(id) = answer(&tasks, &projection) {
                assert_ne!(
                    id,
                    2,
                    "a gate in {} was offered as work for an agent to attempt",
                    gate.name()
                );
            }
        }
    }

    #[test]
    fn a_gate_in_the_middle_runs_what_is_before_it_and_stops() {
        // The drain this task exists for, told once through the two functions that
        // own it: the tasks above the gate run, the gate stops the run, `ack`
        // closes the gate, and the tasks below it run. Every state here is one
        // `apply` after the last, so the story is the machine's own.
        let tasks = plan(3, &[2]);
        assert_eq!(
            answer(&tasks, &states(&[])),
            Answer::Start(1),
            "the queue starts the work above the gate"
        );

        let closed = states(&[(1, TaskState::Done)]);
        assert_eq!(
            answer(&tasks, &closed),
            Answer::Nothing,
            "with the gate next in line the run stops: nothing behind it may start"
        );

        let at_the_gate = apply(&TaskState::Queued, &gate_paused())
            .expect("a run stops at a gate by parking it for a human");
        let stopped = states(&[(1, TaskState::Done), (2, at_the_gate.clone())]);
        assert_eq!(
            answer(&tasks, &stopped),
            Answer::Nothing,
            "standing at the gate is standing still, not drained"
        );

        let passed = apply(&at_the_gate, &gate_acknowledged())
            .expect("the acknowledgement closes the pause that stopped at a gate");
        assert_eq!(
            passed,
            acknowledged(),
            "the gate is closed by who acknowledged it and when, and by nothing else"
        );
        let after = states(&[(1, TaskState::Done), (2, passed)]);
        assert_eq!(
            answer(&tasks, &after),
            Answer::Start(3),
            "and the queue moves past the gate to the work it was holding"
        );
    }

    #[test]
    fn two_active_tasks_are_refused_rather_than_answered() {
        let tasks = plan(3, &[]);
        let projection = states(&[
            (
                1,
                TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: Phase::Implement,
                },
            ),
            (
                2,
                TaskState::Verifying {
                    attempt: AttemptId::new(1),
                },
            ),
            (3, TaskState::Queued),
        ]);
        let outcome = next_runnable(&tasks, &projection);
        let Err(Error::Policy { detail, paths }) = &outcome else {
            panic!("a queue with two runners was answered rather than refused: {outcome:?}");
        };
        assert!(
            paths.is_empty(),
            "a row of the queue broke this rule, not a file: {paths:?}"
        );
        for id in ["task 1 (Running)", "task 2 (Verifying)"] {
            assert!(
                detail.contains(id),
                "the refusal must name the task the caller has to repair: {detail}"
            );
        }
    }

    #[test]
    fn one_active_task_holds_the_slot_from_every_other_task() {
        let tasks = plan(2, &[]);
        let at_work = TaskState::Running {
            attempt: AttemptId::new(1),
            phase: Phase::Implement,
        };

        // The active task is below the candidate, so the ordering check sees it.
        let below = states(&[(1, at_work.clone()), (2, TaskState::Queued)]);
        assert_eq!(
            answer(&tasks, &below),
            Answer::Nothing,
            "work not yet published holds its successor"
        );

        // The active task is above the candidate, where no ordering check looks.
        // Starting the head anyway would put two runners in the queue, which is
        // the invariant the selection exists to keep.
        let above = states(&[(1, TaskState::Queued), (2, at_work)]);
        assert_eq!(
            answer(&tasks, &above),
            Answer::Nothing,
            "the one active slot is held, whoever is holding it"
        );
    }

    #[test]
    fn a_published_but_unclosed_predecessor_holds_the_active_slot() {
        // ADR-0028 names this the one state where the two rules disagree: the
        // ordering check clears a predecessor the remote was proved to hold, and
        // ADR-0027 keeps that state in the active slot until `TaskDone` closes it.
        // Composed, the stricter answer wins — the slot is still occupied.
        let tasks = plan(2, &[]);
        let projection = states(&[(1, published("b7d1f3a")), (2, TaskState::Queued)]);
        assert!(
            check_predecessor(&projection, TaskId::new(2)).is_ok(),
            "work the remote holds clears the way by the ordering check"
        );
        assert_eq!(
            answer(&tasks, &projection),
            Answer::Nothing,
            "and yet the successor may not start while that state still occupies the slot"
        );
    }

    #[test]
    fn what_the_row_concludes_is_not_read_as_an_answer() {
        // `TaskStatus` is what a supervisor concluded, and the journal's read of
        // the state machine is the only evidence of that. A row that claims
        // something its state contradicts is stale, and the state decides.
        let mut tasks = plan(1, &[]);
        tasks[0].status = TaskStatus::Failed;
        assert_eq!(
            answer(&tasks, &states(&[])),
            Answer::Start(1),
            "a row claiming a failure the journal never recorded is the state's to answer"
        );
        tasks[0].status = TaskStatus::HumanGate;
        assert_eq!(
            answer(&tasks, &states(&[])),
            Answer::Start(1),
            "a gate is a `**Gate:**` section, not a status nobody concluded"
        );
    }

    #[test]
    fn an_unfinished_task_holds_the_next_one_back() {
        // Every state that is neither closed nor published, asked as task 1 of a
        // plan whose task 2 is still queued.
        let tasks = plan(2, &[]);
        for state in [
            TaskState::Preflight,
            TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Red,
            },
            TaskState::Remediating {
                attempt: AttemptId::new(2),
                phase: Phase::Green,
            },
            TaskState::Verifying {
                attempt: AttemptId::new(1),
            },
            TaskState::Publishing {
                attempt: AttemptId::new(1),
            },
        ] {
            let projection = states(&[(1, state.clone()), (2, TaskState::Queued)]);
            assert_eq!(
                answer(&tasks, &projection),
                Answer::Nothing,
                "{} holds task 2 where it is",
                state.name()
            );
        }
    }

    /// The index of `TaskState::Queued` in [`every_state`], spelled as a number
    /// because the sweep below indexes the states rather than holding them.
    const QUEUED: usize = 0;

    /// Every state, each carrying the payload its variant demands, in the order
    /// `state.rs` spells the twelve.
    fn every_state() -> [TaskState; 12] {
        [
            TaskState::Queued,
            TaskState::Preflight,
            TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            },
            TaskState::Remediating {
                attempt: AttemptId::new(2),
                phase: Phase::Red,
            },
            TaskState::Verifying {
                attempt: AttemptId::new(1),
            },
            TaskState::Publishing {
                attempt: AttemptId::new(1),
            },
            published("b7d1f3a"),
            TaskState::Done,
            acknowledged(),
            parked(PauseReason::Input),
            failed(),
            TaskState::Cancelled,
        ]
    }

    /// The same twelve by name, written out a second time beside the tables below
    /// so neither can pass by agreeing with the other.
    const STATE_NAMES: [&str; 12] = [
        "Queued",
        "Preflight",
        "Running",
        "Remediating",
        "Verifying",
        "Publishing",
        "PublishedVerified",
        "Done",
        "Acknowledged",
        "Paused",
        "Failed",
        "Cancelled",
    ];

    /// The states that occupy the one active slot, by ADR-0027.
    const ACTIVE: [bool; 12] = [
        false, true, true, true, true, true, true, false, false, false, false, false,
    ];

    /// The states that clear the way for a successor: published, closed,
    /// acknowledged by a human, or dropped by a human. ADR-0028 held
    /// `Acknowledged` back; ADR-0031 added it, and
    /// `an_acknowledged_gate_lets_its_successors_start` is the row that decides.
    const CLEARED: [bool; 12] = [
        false, false, false, false, false, false, true, true, true, false, false, true,
    ];

    /// The states that stop the run wherever they sit: the pause and the failure.
    const STOPPED: [bool; 12] = [
        false, false, false, false, false, false, false, false, false, true, true, false,
    ];

    /// What the queue should answer for `rows` — one index into [`every_state`]
    /// per task, in queue order — with a gate at position `gate`.
    ///
    /// Decided from [`ACTIVE`], [`CLEARED`] and [`STOPPED`] rather than from
    /// `next_runnable`, so the sweep asks a question this test answers on its own.
    fn expected(gate: Option<usize>, rows: [usize; 3]) -> Answer {
        let active = rows.iter().filter(|row| ACTIVE[**row]).count();
        if active > 1 {
            return Answer::Refused;
        }
        if rows.iter().any(|row| STOPPED[*row]) {
            return Answer::Nothing;
        }
        let Some(head) = rows.iter().position(|row| *row == QUEUED) else {
            return Answer::Nothing;
        };
        if gate == Some(head) {
            return Answer::Nothing;
        }
        if rows[..head].iter().any(|row| !CLEARED[*row]) {
            return Answer::Nothing;
        }
        if active > 0 {
            return Answer::Nothing;
        }
        Answer::Start(u32::try_from(head + 1).expect("three tasks fit in a queue position"))
    }

    #[test]
    fn every_combination_of_three_states_answers_as_the_tables_say() {
        let table = every_state();
        for (index, state) in table.iter().enumerate() {
            assert_eq!(
                state.name(),
                STATE_NAMES[index],
                "the sweep names its rows by these states, so a rename has to be made                  here as well as in the state machine"
            );
        }
        assert_eq!(
            ACTIVE.iter().filter(|active| **active).count(),
            6,
            "ADR-0027 fixes the number of active states at six"
        );
        assert_eq!(
            CLEARED.iter().filter(|cleared| **cleared).count(),
            4,
            "the queue proceeds past four states: published, closed, acknowledged, \
             cancelled"
        );
        assert_eq!(
            STOPPED.iter().filter(|stopped| **stopped).count(),
            2,
            "the run stops at a pause or a failure, which are the two states here"
        );

        // Three tasks, every state in each of the three rows, once with no gate
        // and once with a gate in the middle: 3 456 queues decided against the
        // tables rather than against the function. A selector that reads one state
        // wrong, in one row, somewhere in the queue, is caught by one of them.
        for gate_at in [None, Some(1)] {
            let gated = gate_at.map_or_else(Vec::new, |position| vec![position + 1]);
            let tasks = plan(3, &gated);
            let spelled = if gate_at.is_some() {
                "a gate"
            } else {
                "no gate"
            };
            for first in 0..STATE_NAMES.len() {
                for second in 0..STATE_NAMES.len() {
                    for third in 0..STATE_NAMES.len() {
                        let rows = [first, second, third];
                        let projection = states(&[
                            (1, table[first].clone()),
                            (2, table[second].clone()),
                            (3, table[third].clone()),
                        ]);
                        assert_eq!(
                            answer(&tasks, &projection),
                            expected(gate_at, rows),
                            "task 1 ({}), task 2 ({}), task 3 ({}) with {spelled}",
                            STATE_NAMES[first],
                            STATE_NAMES[second],
                            STATE_NAMES[third],
                        );
                    }
                }
            }
        }
    }

    /// Reading the queue out of the database that holds it.
    ///
    /// [`load`](super::load) is the half of this module that knows a queue is a
    /// file somewhere — the rest of the file is pure — so this is the half with
    /// a scratch directory in it. Every fixture is below the system temp
    /// directory, and the [`Project`] handed to `load` is assembled by hand
    /// rather than registered: a loader asks a project for its state directory
    /// and nothing else, so the fixture holds that one path honestly while the
    /// identity and the working copy only keep the shape of a real registration.
    mod loading {
        use std::fs;
        use std::path::Path;

        use tempfile::{TempDir, tempdir};

        use super::plan;
        use crate::queue::load;
        use crate::{Error, EventKind, Journal, Project, Task, TaskId, TaskStatus};

        /// The id a scratch registration carries: sixteen hex characters, a dash,
        /// and sixteen more, which is the shape [`crate::project_id`] prints.
        const AN_ID: &str = "0123456789abcdef-0123456789abcdef";

        /// A scratch directory below the system temp directory: `docs/DESIGN.md`
        /// Conventions forbids a test from writing inside the repository.
        fn scratch() -> TempDir {
            tempdir().expect("a scratch directory below the system temp directory")
        }

        /// The project a first run finds: a state directory registration made,
        /// holding the journal registration opened.
        ///
        /// The journal is made by [`Journal::open_for`] rather than by a
        /// hand-written schema, because what `load` is answerable to is the file
        /// a registration actually leaves behind.
        fn registered(scratch: &Path) -> Project {
            let project = unregistered(scratch);
            fs::create_dir_all(&project.state_dir).expect("a scratch state directory is creatable");
            let journal = Journal::open_for(&project).expect("a state directory takes a journal");
            drop(journal);
            project
        }

        /// The same project with its state directory *not* there — a directory
        /// name that has never been registered.
        fn unregistered(scratch: &Path) -> Project {
            Project {
                root: scratch.join("repository"),
                state_dir: scratch.join("state").join("ktask-rs").join(AN_ID),
                id: AN_ID.to_owned(),
            }
        }

        /// Write `tasks` as the project's queue through a connection that is
        /// closed before this returns: these tests are about what the file holds,
        /// so the rows have to be in the file rather than in a handle.
        fn import(project: &Project, tasks: &[Task]) {
            let mut journal =
                Journal::open_for(project).expect("the project's journal is there to write to");
            journal
                .put_tasks(tasks)
                .expect("an empty queue takes a plan the parser accepted");
            drop(journal);
        }

        /// Every file one state directory holds, with its bytes, in name order.
        ///
        /// Taken only with no journal connection open, which is when SQLite has
        /// checkpointed and deleted its `-wal`/`-shm` pair: what the directory
        /// holds then *is* the durable record, so a row or a file a loader wrote
        /// shows up here instead of hiding in a write-ahead log.
        fn state_dir_files(state_dir: &Path) -> Vec<(String, Vec<u8>)> {
            let mut files = fs::read_dir(state_dir)
                .expect("the state directory is readable")
                .map(|entry| {
                    let path = entry.expect("a state directory entry names a path").path();
                    let name = path
                        .file_name()
                        .expect("a state directory entry has a name")
                        .to_string_lossy()
                        .into_owned();
                    (
                        name,
                        fs::read(&path).expect("a state directory entry is a readable file"),
                    )
                })
                .collect::<Vec<_>>();
            files.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            files
        }

        /// What a call did to one state directory: the files it made, removed or
        /// rewrote between two [`state_dir_files`] snapshots.
        ///
        /// Names rather than the two listings, so a failure says which file
        /// changed instead of printing two databases into the test output.
        fn differences(before: &[(String, Vec<u8>)], after: &[(String, Vec<u8>)]) -> Vec<String> {
            let mut changed = Vec::new();
            for (name, bytes) in after {
                match before.iter().find(|(held, _)| held == name) {
                    None => changed.push(format!("`{name}` was made")),
                    Some((_, held)) if held != bytes => {
                        changed.push(format!("`{name}` was rewritten"));
                    }
                    Some(_) => {}
                }
            }
            for (name, _) in before {
                if !after.iter().any(|(held, _)| held == name) {
                    changed.push(format!("`{name}` was removed"));
                }
            }
            changed
        }

        #[test]
        fn a_project_with_nothing_imported_loads_an_empty_queue() {
            let scratch = scratch();
            let project = registered(scratch.path());

            let loaded = load(&project).expect("a queue nobody has filled is empty, not missing");

            assert!(
                loaded.is_empty(),
                "an untouched queue answers with no rows rather than with an error: every \
                 project starts with one, and reading it is the first thing a run does \
                 (VISION.md section 4)"
            );
        }

        #[test]
        fn the_queue_comes_back_in_the_order_the_plan_was_written() {
            let scratch = scratch();
            let project = registered(scratch.path());
            let authored = plan(4, &[2]);
            import(&project, &authored);

            let loaded = load(&project).expect("a queue of four rows is loadable");

            assert_eq!(
                loaded, authored,
                "the rows come back exactly as the parser made them, gate and all: a loader \
                 that rewrote a row on the way out would leave the plan and the queue \
                 disagreeing about the same task"
            );
            assert_eq!(
                loaded
                    .iter()
                    .map(|task| task.id.get())
                    .collect::<Vec<u32>>(),
                [1, 2, 3, 4],
                "queue order is document order — the position a task had in the plan is its \
                 id, and no row is skipped or duplicated on the way out"
            );
            assert_eq!(
                loaded
                    .iter()
                    .map(|task| task.outcome.as_str())
                    .collect::<Vec<_>>(),
                [
                    "what task 1 changes.",
                    "what task 2 changes.",
                    "what task 3 changes.",
                    "what task 4 changes.",
                ],
                "each row carries the text of its own position, so the order is proved about \
                 the rows and not only about the numbers beside them"
            );
            assert!(
                loaded[1].gate.is_some(),
                "a gate is a section of the body and no column, so it survives the trip \
                 through the database and is still a gate on the way out"
            );
        }

        #[test]
        fn loading_the_queue_changes_nothing_in_the_state_directory() {
            let scratch = scratch();
            let project = registered(scratch.path());
            import(&project, &plan(3, &[]));

            let before = state_dir_files(&project.state_dir);
            let loaded = load(&project).expect("a queue of three rows is loadable");
            let after = state_dir_files(&project.state_dir);

            assert_eq!(
                loaded.len(),
                3,
                "the queue the file holds is the queue handed back"
            );
            assert_eq!(
                differences(&before, &after),
                Vec::<String>::new(),
                "reading the queue writes nothing back: no status row, no queue file, no \
                 journal record. A task's status is its `TaskState`, recorded as an event like \
                 every other change, and the queue itself was written once by the import \
                 (VISION.md section 4, docs/DESIGN.md Database schema)"
            );
        }

        #[test]
        fn a_load_reads_what_the_file_holds_now_rather_than_what_the_last_load_saw() {
            let scratch = scratch();
            let project = registered(scratch.path());

            assert!(
                load(&project)
                    .expect("an empty queue is loadable")
                    .is_empty(),
                "the queue is empty before the import"
            );

            let authored = plan(3, &[]);
            import(&project, &authored);

            assert_eq!(
                load(&project).expect("an import is visible to the next read"),
                authored,
                "the queue lives in the file, so a loader that kept what it read last time \
                 would run a project whose queue had moved on since"
            );
            assert_eq!(
                load(&project).expect("the queue is loadable twice over"),
                authored,
                "two reads of an unchanged queue agree, in the same order"
            );
        }

        #[test]
        fn a_row_comes_back_without_a_status_however_the_journal_behind_it_has_moved() {
            let scratch = scratch();
            let project = registered(scratch.path());
            import(&project, &plan(3, &[]));
            let mut journal =
                Journal::open_for(&project).expect("the journal is there to record to");
            journal
                .append(Some(TaskId::new(1)), &EventKind::PreflightStarted)
                .expect("a record the catalog holds is appended");

            let loaded = load(&project).expect("a queue whose head has started is still loadable");
            let records = journal
                .events_for(TaskId::new(1))
                .expect("the record just appended is readable");
            drop(journal);

            assert!(
                matches!(records[0].kind, EventKind::PreflightStarted),
                "the journal really has moved the head on: its row 1 record is {:?}",
                records[0].kind
            );
            assert_eq!(
                loaded
                    .iter()
                    .map(|task| task.status)
                    .collect::<Vec<TaskStatus>>(),
                [TaskStatus::Pending; 3],
                "a queue row holds what was asked and no status (ADR-0019): status is the \
                 `TaskState` the journal implies, so a loader neither folds it into the rows \
                 nor writes it back into them"
            );
        }

        #[test]
        fn a_project_with_no_state_directory_is_refused_and_left_without_one() {
            let scratch = scratch();
            let project = unregistered(scratch.path());

            let error = load(&project)
                .expect_err("a project that was never registered has no queue to read");

            assert!(
                matches!(error, Error::Database(_)),
                "the filesystem refused to open a journal that is not there, and says so: \
                 {error}"
            );
            assert!(
                !project.state_dir.exists(),
                "loading makes nothing: a state directory made here would be group-readable \
                 and would look exactly like a registration, which is `register`'s job alone \
                 (VISION.md section 11)"
            );
        }
    }
}
