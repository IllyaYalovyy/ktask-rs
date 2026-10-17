//! Explicit --demo fixtures; never loaded into persistent application storage.
use crate::presentation::*;
use std::collections::BTreeMap;

pub fn dataset(name: &str) -> (Vec<Task>, Vec<Task>) {
    if name == "Empty" {
        return (Vec::new(), Vec::new());
    }
    let titles = [
        "Queue pagination",
        "Keyboard navigation",
        "Search matching",
        "Failure recovery",
        "Workspace cleanup",
        "Instance isolation",
    ];
    let count = if matches!(name, "Guided" | "Waits" | "Approvals") {
        3
    } else {
        120
    };
    let mut queue: Vec<_> = (1..=count)
        .map(|id| {
            Task::pending(
                id,
                &format!("{} · case {id}", titles[(id - 1) % titles.len()]),
            )
        })
        .collect();
    queue[0].revisions[0].title = "Recover a failed check".into();
    queue[0].revisions[0].goal = "Include every task on the final queue page.".into();
    queue[0].revisions[0].check = "All 240 tasks are reachable.".into();
    let mut logs: Vec<_> = (1..=64)
        .map(|n| format!("PASS  queue fixture {n:02}"))
        .collect();
    logs.extend([
        "FAIL  final_page_includes_last_row".into(),
        "Expected 240 tasks; received 239.".into(),
        "Execution stopped. Diagnose the boundary error.".into(),
    ]);
    let failed_revision = queue[0].definition().clone();
    queue[0].attempts.push(Attempt {
        logs,
        outcome: "Failed: final row omitted".into(),
        step: 6,
        cost_cents: 8,
        elapsed_ms: 2400,
        state: Status::Failed,
        ..Attempt::start(failed_revision, 1)
    });
    queue[1].revisions[0].title = "Approve the queue interaction".into();
    queue[1].revisions[0].kind = Kind::Human;
    queue[1].revisions[0].goal = "Inspect the queue and acknowledge the result.".into();
    queue[1].revisions[0].check = "A person confirms the interaction works.".into();
    queue[2].revisions[0].title = "Choose the export format".into();
    queue[2].needs_question = true;
    if name == "Waits" {
        queue[0].attempts.clear();
        queue[0].needs_rate_limit = true;
        queue[0].revisions[0].repaired = true;
        queue[0].revisions[0].title = "Resume after provider rate limit".into();
    }
    if name == "Approvals" {
        queue[0].attempts.clear();
        queue[0].revisions[0].state = RevisionState::Draft;
        queue[0].revisions[0].actor = "AI: planner".into();
        queue[0].revisions[0].title = "Review planned work".into();
    }
    let history = if count == 3 {
        Vec::new()
    } else {
        (121..=240)
            .map(|id| {
                let mut task = Task::pending(
                    id,
                    &format!("{} · case {id}", titles[(id - 1) % titles.len()]),
                );
                task.resolution = Some(Status::Done);
                task.attempts.push(Attempt {
                    logs: vec![
                        "Executor: implemented the change".into(),
                        "Checks: passed".into(),
                        "Review: approved; change recorded (mock)".into(),
                    ],
                    outcome: "Completed".into(),
                    step: 6,
                    cost_cents: 3,
                    elapsed_ms: 2400,
                    state: Status::Done,
                    ..Attempt::start(task.definition().clone(), id)
                });
                task
            })
            .collect()
    };
    (queue, history)
}

pub fn files() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("/demo/import.tsv".into(), "AI\tImported check\tVerify the imported task\tChecks pass\nHuman\tImported review\tReview the result\tPerson approves".into()),
        ("/demo/invalid.tsv".into(), "invalid task data".into()),
        ("/demo/existing.tsv".into(), "AI\tExisting task\tKeep until overwrite confirmed\tCheck passes".into()),
    ])
}
